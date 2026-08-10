;;;; project.lisp — where a project starts, and what "build it" means there.
;;;;
;;;; Wave 2 of the Common Lisp migration, and deliberately only the half of
;;;; `crates/app/src/project.rs' that is *policy*. The split is the boundary in
;;;; `docs/boundary.org', applied verb by verb:
;;;;
;;;;   here    project-root, project-dired, project-compile, project-test —
;;;;           four questions answered by a handful of `probe-file' calls and a
;;;;           table, once per command.
;;;;   Rust    project-find-file, project-find-dir, project-forget — a walk of
;;;;           the whole tree behind a cache that has to answer between two
;;;;           keystrokes; project-switch and project-open, which need a prompt
;;;;           seeded with candidates core owns.
;;;;
;;;; The table below is the reason this moved. `cargo build' being what a Rust
;;;; project's `SPC p c' runs was a `match' arm in `crates/project', so wanting
;;;; `cargo build --release', or `zig build' for a `build.zig', meant editing
;;;; Rust and recompiling. It is now a list you can PUSH onto from your init,
;;;; which is the whole definition of policy this migration is sorted by.

(in-package :zemacs)

(defparameter *project-roots*
  '((".project" "project")
    (".git"     "git")
    (".hg"      "hg")
    (".svn"     "svn"))
  "Files whose presence makes a directory the top of a project, in precedence
order. A hand-dropped `.project' outranks a repository, which is how a package
inside a monorepo becomes its own project when — and only when — someone says
so.

The label is what `project-root' reports: `.git' reads as a directory and `git'
reads as a fact about the project.")

(defparameter *project-builders*
  '(("Cargo.toml"     "cargo"  "cargo build"     "cargo test")
    ("package.json"   "npm"    "npm run build"   "npm test")
    ;; `python3' and not `python': on a machine with both, `python' is either
    ;; python 2 or missing entirely.
    ("pyproject.toml" "python" "python3 -m build" "python3 -m pytest")
    ("go.mod"         "go"     "go build ./..."  "go test ./...")
    ;; No target for the build: a Makefile's default target *is* the build, and
    ;; it is the one thing every Makefile has.
    ("Makefile"       "make"   "make"            "make test"))
  "(MARKER LABEL BUILD TEST) per kind of project, in precedence order.

Two jobs, which is why it is one list. A build file is also a *root* when there
is no repository anywhere above it — a checked-out crate with no `.git' is still
a project — and it is what `project-compile' and `project-test' read to decide
what to run. Rows are shell lines rather than program-plus-arguments because
they are handed to `sh -c' with a `cd' in front; see `%project-run'.

The hook: `(push '(\"build.zig\" \"zig\" \"zig build\" \"zig build test\")
*project-builders*)' in your init, and zig projects work.")

(defun %project-marker (dir table)
  "The row of TABLE whose marker file is in DIR, or NIL.

`merge-pathnames' and not string concatenation because ECL parses a leading dot
correctly — `(merge-pathnames \".git\" #p\"/x/\")' is `/x/.git' and not a file
of type `git' — and `probe-file' answers for a directory as readily as for a
file, which `.git' needs it to."
  (find-if (lambda (row) (probe-file (merge-pathnames (first row) dir))) table))

(defun %ancestors (start)
  "START's directory and every directory above it, nearest first.

Walks the *directory components* rather than repeatedly taking a pathname's
parent, for `%makefile-near''s reason: shortening a list is total, where
climbing by pathname has to decide when it has reached the root and gets there
by asking a question the root itself answers badly."
  (let ((parts (pathname-directory (merge-pathnames start))))
    (loop for n from (length parts) downto 1
          collect (make-pathname :directory (subseq parts 0 n)
                                 :name nil :type nil))))

(defun %project-start ()
  "The directory to climb from: the live buffer's file, else the process's own."
  (let* ((file (buffer-file-name))
         (real (and file (probe-file file))))
    (cond ((null file) *default-pathname-defaults*)
          ;; A dired buffer's \"file\" is a directory, and `%directory-of' would
          ;; climb from its parent — so `SPC p r' inside a repository's own dired
          ;; would answer about whatever contains it.
          ((and real (null (pathname-name real))) real)
          (t (%directory-of (pathname file))))))

(defun %project-at (&optional start)
  "(ROOT LABEL) for the project holding START, or NIL if there is none.

*The deepest repository wins, and a build file only counts when there is no
repository anywhere above it.* So a Cargo workspace member inside a git
checkout resolves to the checkout: the interesting operations are
repository-shaped — a project search should cross crate boundaries, because
that is where the definition being hunted lives. The cost is that a genuinely
independent sub-project resolves too high, and the fix for that is a `.project'
file in it, which `*project-roots*' checks first.

Same rule `crates/project' still applies for `project-find-file', because these
two must not disagree about which tree you are in."
  (let ((build nil))
    (dolist (dir (%ancestors (or start (%project-start))))
      (let ((root (%project-marker dir *project-roots*)))
        (when root (return-from %project-at (list dir (second root)))))
      ;; Remembered, not returned: a `.git' further up still outranks it, so the
      ;; *deepest* build file wins only among build files.
      (unless build
        (let ((row (%project-marker dir *project-builders*)))
          (when row (setf build (list dir (second row)))))))
    build))

(defun %project-command (root which)
  "The shell line WHICH (`:build' or `:test') runs in ROOT, or NIL.

Read from the root rather than from the marker that made it one, which is
usually not the same file: a Cargo workspace is identified by its `.git', and
`cargo build' is still the answer. Costs one `probe-file' per builder, and only
when someone asks to compile."
  (let ((row (%project-marker root *project-builders*)))
    (and row (if (eq which :build) (third row) (fourth row)))))

(defparameter *project-none*
  "not in a project — no .git, Cargo.toml or the like above this file"
  "What every verb here says when the climb reaches / with nothing to show. One
string because saying it four different ways would read as four different
failures.")

(defun %project-run (which name)
  "Run this project's WHICH command in an output pane called NAME.

`output:' rather than `shell:' or a line typed into your shell, which is what
this used to be. A build is something you *read*: the pane opens beside what you
were working on, the editor keeps the keyboard — so the motions and the searches
work while cargo is still printing — `q' dismisses it, and pressing the key again
re-runs into the same pane instead of stacking a second. `project-make' made the
same argument first and this is now consistent with it.

Through `sh -c \"cd ROOT && …\"' because the child otherwise starts wherever the
buffer you were in lives. `cargo' and `make' climb to find their own manifest and
would not notice; `npm' does not climb, and would build the wrong package in a
monorepo.

ponytail: ROOT is interpolated into a double-quoted shell word, so a project
directory containing a `\"' or a `$' would break the line. The upgrade is a
`%shell-quote' that wraps in single quotes and escapes those; nobody has a
repository called `my\"repo'."
  (let ((found (%project-at)))
    (cond
      ((null found) (message *project-none*))
      (t
       (let* ((root (namestring (first found)))
              (command (%project-command (first found) which)))
         (if command
             (terminal (format nil "output:~a:/bin/sh -c \"cd ~a && ~a\""
                               name root command))
             (message (format nil "no ~a command for a ~a project"
                              name (second found)))))))))

(defun project-root ()
  "Say where this project is, and what identified it. Bound to `SPC p r'."
  (let ((found (%project-at)))
    (message (if found
                 (format nil "~a (~a)" (namestring (first found)) (second found))
                 *project-none*)))
  nil)

(defun project-dired ()
  "Open the project root in dired. Bound to `SPC p d'."
  (let ((found (%project-at)))
    (if found
        (dired (namestring (first found)))
        (message *project-none*)))
  nil)

(defun project-compile ()
  "Build this project, in an output pane. Bound to `SPC p c'."
  (%project-run :build "compile"))

(defun project-test ()
  "Run this project's tests, in an output pane. Bound to `SPC p t'."
  (%project-run :test "test"))
