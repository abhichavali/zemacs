;;;; project.lisp — where a project starts, what "build it" means there, and
;;;; every picker that opens onto one.
;;;;
;;;; Waves 2 and 3 of the Common Lisp migration. Wave 2 moved the four questions
;;;; answered by a handful of `probe-file' calls and a table; wave 3 moved the
;;;; four pickers, which needed something first. The split now:
;;;;
;;;;   here    every `project-' verb but one.
;;;;   Rust    the tree walk behind `project-files' / `project-dirs', which has
;;;;           to answer between two keystrokes and is a cache rather than a
;;;;           decision; and `project-forget', which drops that cache.
;;;;
;;;; What wave 3 needed was a way to *seed a prompt with candidates core owns*.
;;;; `docs/boundary.org' listed the absence of one as the reason these four
;;;; could not move: `completing-read' carried its own candidates and could not
;;;; borrow the app's, and a project's file list is exactly the list you must not
;;;; carry into the image and back out again. Three verbs closed it —
;;;; `prompt-text', `prompt-label' and `prompt-source' — and the third is the
;;;; interesting one: Lisp names the list it wants and the app fills the picker
;;;; directly, so the label, the callback and what accepting one *means* are
;;;; here, and fifty thousand paths never cross the shim.
;;;;
;;;; The table below is the reason wave 2 moved. `cargo build' being what a Rust
;;;; project's `SPC p c' runs was a `match' arm in `crates/project', so wanting
;;;; `cargo build --release', or `zig build' for a `build.zig', meant editing
;;;; Rust and recompiling. It is now a list you can PUSH onto from your init,
;;;; which is the whole definition of policy this migration is sorted by.
;;;;
;;;; Remote projects. A buffer visiting `/ssh:host:/src/app/main.rs' is in the
;;;; project at `/ssh:host:/src/app/', and every verb here works on it: the
;;;; climb is one ssh round trip (`%remote-probe') asking about the same two
;;;; tables, the pickers are filled from a listing the app fetches off the
;;;; frame, `project-compile' and `project-shell' start their child on the host
;;;; through `ssh -t', and `SPC s p' searches there. The one exception is
;;;; `project-grep', whose listing is edited with local file IO; it says so.

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

(defun %remote-p (name)
  "True for a `/ssh:host:/path' name: a file on another machine, which no
`probe-file' here can answer for and no pathname should be made of."
  (and (stringp name) (> (length name) 5) (string= "/ssh:" name :end2 5)))

(defparameter *remote-projects* (make-hash-table :test #'equal)
  "Remote file or directory -> its (ROOT LABEL), or NIL for none. The climb is a
round trip and the answer does not move under a file, so each name pays once.

ponytail: kept for the session — `project-forget' is the app's and does not
reach this, so a `.git' made on the host after the file was opened is seen after
a restart or a `(clrhash *remote-projects*)'.")

(defparameter *project-trouble* nil
  "Why the last climb found nothing, when the reason was a host that did not
answer rather than a tree with no marker in it. Said instead of
`*project-none*', which would be a confident wrong answer about a project that
is there.")

(defun %project-none ()
  "What a verb says when there is no project to act on."
  (or *project-trouble* *project-none*))

(defun %marker-names (&rest tables)
  "The marker file names of TABLES, one per line, for `%remote-probe'."
  (format nil "~{~a~%~}" (mapcar #'first (apply #'append tables))))

(defun %remote-row (dir hits table)
  "The first row of TABLE whose marker the probe found in DIR."
  (find-if (lambda (row) (member (list dir (first row)) hits :test #'equal)) table))

(defun %remote-project-at (name)
  "`%project-at' for NAME on another machine: (ROOT LABEL), ROOT a tramp name
ending in `/', or NIL.

One `%remote-probe' asks the host which of every marker in both tables sits in
each directory above NAME, and the rule below — deepest repository, a build
file only without one — is applied here to the answer, from the same tables, so
a row pushed onto either from an init counts over ssh too. A host that did not
answer is asked again next time, and `%project-none' says why it did not; an
answer is remembered."
  (multiple-value-bind (known seen) (gethash name *remote-projects*)
    (if seen
        known
        (let ((hits (%remote-probe name (%marker-names *project-roots*
                                                       *project-builders*))))
          (if (stringp hits)
              (progn (setf *project-trouble* hits) nil)
              (setf (gethash name *remote-projects*)
                    (let ((build nil))
                      (dolist (dir (remove-duplicates (mapcar #'first hits)
                                                      :test #'string= :from-end t)
                                   build)
                        (let ((root (%remote-row dir hits *project-roots*)))
                          (when root (return (list dir (second root)))))
                        (unless build
                          (let ((row (%remote-row dir hits *project-builders*)))
                            (when row (setf build (list dir (second row))))))))))))))

(defun %root-name (root)
  "ROOT as the string a verb or a message wants. A remote root already is one."
  (if (stringp root) root (namestring root)))

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
two must not disagree about which tree you are in.

A remote START — or a remote buffer, with none — is `%remote-project-at''s, and
its ROOT is a string rather than a pathname."
  (setf *project-trouble* nil)
  (let ((file (or start (buffer-file-name))))
    (when (%remote-p file)
      (return-from %project-at (%remote-project-at file))))
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
when someone asks to compile — or, for a remote ROOT, one round trip."
  (let ((row (if (%remote-p root)
                 (let ((hits (%remote-probe root (%marker-names *project-builders*))))
                   (and (listp hits) (%remote-row root hits *project-builders*)))
                 (%project-marker root *project-builders*))))
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

At ROOT because the child otherwise starts wherever the buffer you were in
lives. `cargo' and `make' climb to find their own manifest and would not notice;
`npm' does not climb, and would build the wrong package in a monorepo. `cd:' is
the terminal's `default-directory' for the one verb after it, so ROOT is a
directory handed over and never a word spliced into the shell line — which is
also what lets a remote ROOT run the build on its host, through `ssh -t'."
  (let ((found (%project-at)))
    (cond
      ((null found) (message (%project-none)))
      (t
       (let ((command (%project-command (first found) which)))
         (cond
           (command
            (terminal (format nil "cd:~a" (%root-name (first found))))
            (terminal (format nil "output:~a:/bin/sh -c ~s" name command)))
           (t (message (format nil "no ~a command for a ~a project"
                               name (second found))))))))))

(defun project-root ()
  "Say where this project is, and what identified it. Bound to `SPC p r'."
  (let ((found (%project-at)))
    (message (if found
                 (format nil "~a (~a)" (%root-name (first found)) (second found))
                 (%project-none))))
  nil)

(defun project-dired ()
  "Open the project root in dired. Bound to `SPC p d'."
  (let ((found (%project-at)))
    (if found
        (dired (%root-name (first found)))
        (message (%project-none))))
  nil)

(defun project-compile ()
  "Build this project, in an output pane. Bound to `SPC p c'."
  (%project-run :build "compile"))

(defun project-test ()
  "Run this project's tests, in an output pane. Bound to `SPC p t'."
  (%project-run :test "test"))

;;; ---------------------------------------------------------------------------
;;; The pickers
;;;
;;; Each is the same three moves: open a `completing-read' with no candidates,
;;; name the list the app should pour into it, and say what accepting one does.
;;; The list never enters the image — see the header — so a repository with fifty
;;; thousand files costs the same here as one with ten.
;;;
;;; The order matters and is the only subtlety. `completing-read' and
;;; `prompt-source' are both `%do' verbs, so they are applied in the order they
;;; were emitted: the prompt exists by the time the app goes looking for one to
;;; fill.

(defun %project-name (root)
  "ROOT's own directory name, for a prompt label — `zemacs', not the whole path."
  (if (stringp root)
      (let* ((dir (string-right-trim "/" root))
             (cut (position #\/ dir :from-end t)))
        (if cut (subseq dir (1+ cut)) dir))
      (or (car (last (pathname-directory root))) (namestring root))))

(defun %project-pick (source label callback)
  "Ask LABEL over the app's SOURCE list, and call CALLBACK with the answer.

CALLBACK is spared the two checks every one of these wants: a cancelled prompt
answers NIL, and a prompt with nothing highlighted answers whatever was typed,
which for a picker over paths may be the empty string."
  (completing-read label nil
                   (lambda (answer)
                     (when (and answer (plusp (length answer)))
                       (funcall callback answer))))
  (%do "prompt-source" source 0 0)
  nil)

(defun project-find-file ()
  "Pick a file from anywhere in this project. Bound to `SPC p f'.

The list is the app's cached walk of the tree, which is what makes it answer
between two keystrokes. What is *here* is the part worth bending: which project
you are in, what the prompt says, and that accepting one opens it — swap
`find-file' for a split and `SPC p f' opens in the other window."
  (let ((found (%project-at)))
    (if (null found)
        (message (%project-none))
        (let ((root (%root-name (first found))))
          (%project-pick (format nil "project-files ~a" root)
                         (format nil "~a: " (%project-name (first found)))
                         #'find-file))))
  nil)

(defun project-find-dir ()
  "Pick a directory from anywhere in this project. Bound to `SPC p D'.

The same listing folded up to the directories holding those files, so it costs
one pass over a list that is already cached rather than a second walk. Accepting
one opens it as a directory, which is dired."
  (let ((found (%project-at)))
    (if (null found)
        (message (%project-none))
        (let ((root (%root-name (first found))))
          (%project-pick (format nil "project-dirs ~a" root)
                         (format nil "~a directory: " (%project-name (first found)))
                         #'find-file))))
  nil)

(defun project-switch ()
  "Pick from the projects visited before. Bound to `SPC p p'.

They open as directories, and a directory opens dired, which is where you would
want to land.

An ordinary `completing-read' rather than a `prompt-source', because this list
is a few dozen paths and it is worth having *in the image*: with nothing
remembered, switching project simply is browsing for one, and only a caller that
can see the list is empty can hand over to `project-open' instead of opening a
picker onto nothing."
  (let ((seen (project-recent)))
    (cond
      ((null seen)
       (message "no projects visited yet — type a path to one")
       (project-open))
      (t (completing-read "Switch to project: " seen
                          (lambda (answer)
                            (when (and answer (plusp (length answer)))
                              (find-file answer)))))))
  nil)

(defparameter *project-grep-limit* 2000
  "Most rows `project-grep' will put in the listing. Past this you are reading a
concordance rather than a search result, and the thing to do is narrow the
pattern.")

(defun project-grep ()
  "Search the project and put every hit in a buffer. Bound to `SPC p g'.

The other half of `search-project', which is the same ripgrep through a
*picker*: a picker offers you one of its candidates and throws the rest away,
and this is for the times when the list itself is the answer — reading every
call site, or replacing across them, which is `r' in the listing.

Absolute paths, because the rows outlive the search: `find-file-at' opens one
long after the process's own directory has stopped being relevant, and
`xref-replace' writes to them. Passing the root as ripgrep's *path argument* is
what makes them absolute — it prints what it was given."
  (let ((found (%project-at)))
    (cond
      ((null found) (message (%project-none)))
      ;; The listing's `r' rewrites files with local file IO, which a remote
      ;; row would send to a path on *this* machine. The picker is the same
      ;; ripgrep, run on the host.
      ((stringp (first found))
       (message "project-grep is local only — SPC s p searches a remote project"))
      (t
        (let ((root (namestring (first found))))
          (read-string "Search project: "
            (lambda (pattern)
              (when (and pattern (plusp (length pattern)))
                (multiple-value-bind (out status)
                    (run-process "rg" (list "--line-number" "--no-heading"
                                            "--color=never" "--smart-case"
                                            "--" pattern root))
                  (let ((rows (remove "" (split-string (or out "") #\Newline)
                                      :test #'string=)))
                    (cond
                      ;; ripgrep exits 1 for "no matches", which is not a
                      ;; failure — an empty output with a clean exit is the same
                      ;; answer said twice.
                      ((and (null rows) (eq status :exited))
                       (message (format nil "no matches for ~a" pattern)))
                      ((null rows)
                       (message (format nil "rg: ~a" (or out status))))
                      (t
                       (let ((rows (if (> (length rows) *project-grep-limit*)
                                       (subseq rows 0 *project-grep-limit*)
                                       rows)))
                         (xref-show (format nil "~a hit~:p for ~a"
                                            (length rows) pattern)
                                    rows)))))))))))))
  nil)

(defun project-open ()
  "Type the path of a directory anywhere on the filesystem. Bound to `SPC p o'.

The hole this fills: `project-switch' can only offer roots you have been in and
`project-find-file' is scoped to one of them, so a project you had never opened
was unreachable from the project keymap — which is the only place anyone looks
for it.

This is the editor's own file picker rather than a `completing-read', because
the app completes it from the filesystem one directory at a time: typing or Tab
descends, and a leading `/' starts again from the root. Seeded at an *expanded*
home rather than a literal `~/', because those completions come back as absolute
paths and the fuzzy filter would match none of them against a tilde.

Another machine is the same prompt: type `/ssh:' and it offers the hosts in
`~/.ssh/config', and past the host's colon it lists that host's directories the
way it lists this one's — asked off the frame, so a slow host never stalls a
keystroke.

Accepting a directory opens dired on it *and* records it as a project, because
the app remembers every directory it opens — so this prompt is only ever needed
for the first visit and `project-switch' covers the rest."
  (open-prompt "file")
  (%do "prompt-label" "Open directory: " 0 0)
  (%do "prompt-text" (namestring (user-homedir-pathname)) 0 0)
  nil)

;;; ---------------------------------------------------------------------------
;;; Running things at the root
;;;
;;; Both go through `cd:', the terminal's one-verb `default-directory', so the
;;; root is a directory handed to the child and never text in a command line —
;;; and a remote root is a child on the host, through `ssh -t', for free.

(defun project-shell ()
  "A shell at the project root. Bound to `SPC p s'.

Pressed again it shows the same shell rather than starting another — the one
started *at this root*, so two projects keep two shells. On a remote project it
is a login shell on the host, in the root, over the connection the files
already came through."
  (let ((found (%project-at)))
    (cond
      ((null found) (message (%project-none)))
      (t (terminal (format nil "cd:~a" (%root-name (first found))))
         (terminal "open"))))
  nil)

(defun project-shell-command ()
  "Run one shell line at the project root and say what it printed. Bound to
`SPC p !'.

`:!' with the root as its directory: run and reported in the echo area, no
buffer left behind — on the host, when the project is remote."
  (let ((found (%project-at)))
    (if (null found)
        (message (%project-none))
        (let ((root (%root-name (first found))))
          (read-string (format nil "! in ~a: " (%project-name (first found)))
                       (lambda (line)
                         (when (and line (plusp (length line)))
                           (terminal (format nil "cd:~a" root))
                           (terminal (format nil "shell:~a" line))))))))
  nil)

;;; Here rather than in `init.lisp' so that every config has them — the
;;; shipped keys for the rest of `SPC p' live in the init a user copies, and a
;;; copy made before these existed would never learn them. Emacs' own letters:
;;; `project-shell' is `C-x p s' and `project-shell-command' is `C-x p !'.
(define-leader "SPC p s" "project-shell")
(define-leader "SPC p !" "project-shell-command")
