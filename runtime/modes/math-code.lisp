;;;; math-code.lisp — a programming problem, as somewhere to work.
;;;;
;;;; `math.lisp' can already tell you that the problem under point is a
;;;; `programming' one and hand you the `#+begin_src python' block it carries.
;;;; This is what happens next: the block becomes a file you can edit, an
;;;; interpreter that can import numpy, a window beside the question, and one
;;;; key that runs it. Four things, none of which the learner should have to ask
;;;; for by name.
;;;;
;;;; Nothing here decides whether a program is *right*. It runs it and shows you
;;;; what came out, which is the same refusal `math.lisp' states at length about
;;;; `:ZEMACS_STATUS:' and for the same reason: a maths curriculum whose editor
;;;; grades you is one you stop trusting the first time it is wrong about a
;;;; correct answer. `math-toggle-status' is still the only thing that writes a
;;;; status, and it is still bound to a key rather than to an event.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; WHERE THINGS GO
;;;;
;;;; For a curriculum at `~/maths/linear-algebra.org':
;;;;
;;;;   ~/maths/linear-algebra/                   the programs, one per problem
;;;;   ~/maths/linear-algebra/gaussian.py        `:tangle gaussian.py'
;;;;   ~/maths/linear-algebra/.venv/             the interpreter and its packages
;;;;   ~/maths/linear-algebra/.zemacs-setup.sh   how the environment was built
;;;;   ~/maths/linear-algebra/.zemacs-setup.log  what it said while building
;;;;
;;;; **One directory beside the .org file, named from it.** A curriculum is a
;;;; project and a project is a directory; a folder that says which curriculum
;;;; it belongs to means two curricula in one directory do not collide, and it
;;;; means the whole thing — questions, answers and environment — moves, copies
;;;; and deletes as a unit. `:tangle' names the file inside it, and
;;;; `docs/curriculum.org' already requires those names to be unique across the
;;;; file, which is what makes a flat directory enough.
;;;;
;;;; **One venv per curriculum, not per problem.** A virtual environment with
;;;; numpy in it is on the order of a hundred megabytes and half a minute; a
;;;; curriculum has fifty problems. Per problem that is gigabytes and an
;;;; afternoon, spent isolating exercises that differ by whether they also
;;;; import sympy. So packages *accumulate* into one environment as problems ask
;;;; for them, and the record of what is in it lives inside the venv itself —
;;;; delete `.venv' and the record goes with it, so the two cannot disagree.
;;;;
;;;; **`.venv' inside that directory, and not under `~/.cache'.** A cache
;;;; directory keyed by a hash of the .org path is tidier and worse in three
;;;; ways: nothing else knows to look there, so a language server, a `pyright'
;;;; or a shell you `cd' into finds no interpreter; you cannot tell which venv
;;;; belongs to what without reversing a hash; and moving the .org file orphans
;;;; a hundred megabytes nothing will ever collect. `.venv' next to the code is
;;;; the convention every Python tool already implements, and `python -m venv'
;;;; has written `.venv/.gitignore' itself since 3.11 — so the one real cost, a
;;;; directory you did not mean to commit, is already paid for.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; TANGLING IS WRITE-ONCE
;;;;
;;;; The template goes to disk **only when the file is not there**. Re-tangling
;;;; over an afternoon's work is the one unrecoverable thing this file could do,
;;;; and it would do it silently, on the keystroke you press to *look* at the
;;;; problem. So opening is idempotent and never writes over anything, and
;;;; putting the template back is a separate command with its own name,
;;;; `math-code-reset', which asks first — and which, when the program is open,
;;;; rewrites the *buffer* rather than the file, so `u' undoes it.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; NEVER BLOCKING THE IMAGE
;;;;
;;;; Building a venv and pip-installing numpy takes tens of seconds. The Lisp
;;;; thread evaluates one queued form at a time and nothing preempts it — see
;;;; the ceiling at the top of `repl.lisp' and in `docs/boundary.org' — so a
;;;; `run-program' that waited for pip would mean every keystroke's Lisp landing
;;;; a minute late. `tutor.lisp' polls a child exactly like that on purpose and
;;;; gives it five seconds; a package install is two orders of magnitude past
;;;; that.
;;;;
;;;; So the build is `ext:run-program' with `:wait nil' and *nothing waits for
;;;; it*. Three consequences, all deliberate:
;;;;
;;;; - The child is `/bin/sh' running a script this file writes, and the script
;;;;   redirects its own output to a log on its first line. Nothing has to drain
;;;;   a pipe, so nothing can deadlock on a full one — which is the failure the
;;;;   careful draining loop in `tutor.lisp' exists to avoid.
;;;; - Whether it has finished is asked on `*point-moved-functions*', which
;;;;   costs one special-variable read per cursor movement when no build is
;;;;   running. There is no timer in this editor to hang it off instead, and
;;;;   adding one for the sake of a message would be a change in Rust.
;;;; - Success is decided by looking at the *filesystem* — is there an
;;;;   interpreter, are the packages recorded — rather than by an exit status.
;;;;   That is the question everything else here asks anyway, so there is one
;;;;   answer rather than two that can come to disagree.
;;;;
;;;; The rejected alternative was building it in a terminal buffer, where pip's
;;;; own output would be the progress report. It is genuinely attractive and it
;;;; is wrong here: `terminal-run' takes the window it is called from and hands
;;;; the keyboard to the child, so the build would demolish the split we opened
;;;; the problem to arrange, on the keystroke that arranged it.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; RUNNING *IS* A TERMINAL
;;;;
;;;; The other direction, and the same argument turned inside out. Running the
;;;; program is something you asked for and want to watch, and a student's
;;;; program does things a captured pipe cannot carry: `input()' wants a
;;;; keyboard, a long loop wants to print as it goes, `C-c' wants to interrupt
;;;; it, a traceback wants its colours, and matplotlib wants a process it can
;;;; open a window from. `crates/term' already runs a program on a PTY in a
;;;; buffer and `ai.lisp' already shows how to ask for one — a verb string — so
;;;; this is a dozen lines rather than an output pane of its own.
;;;;
;;;; It runs through a script for the same reason the build does: `terminal-run'
;;;; splits its command line on whitespace and so cannot express a `cd', and a
;;;; program's working directory has to be the directory its data files are in.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; The vocabulary
;;;
;;; DEFPARAMETER throughout, as in `math.lisp': reloading the config should put
;;; the shipped answer back, since a stale override is indistinguishable from a
;;; file that does not parse.

(defparameter *math-code-python* "python3"
  "The interpreter a virtual environment is built *with*, looked up on $PATH.

A machine whose Python is `python3.12', or a pyenv shim, sets this and
everything else follows: nothing below names an interpreter except through here
and through the venv this one produces.")

(defparameter *math-code-packages* '("numpy")
  "What every environment gets before any problem asks for anything.

numpy, because a maths curriculum whose very first skeleton says `import numpy
as np' and then makes you install it has already failed at the thing this file
is for. A problem's `:ZEMACS_PACKAGES:' is added to this rather than replacing
it.")

(defparameter *math-code-venv-name* ".venv/"
  "The environment's directory, relative to the curriculum's own. Trailing
slash on purpose: a directory component parses the same in every Common Lisp,
where a leading dot in a *file* name does not.")

(defparameter *math-code-record-name* ".zemacs-packages"
  "Inside the venv: the packages installed into it, one per line. Inside, so
that deleting the environment deletes the claim that it exists.")

(defparameter *math-code-setup-name* ".zemacs-setup.sh")
(defparameter *math-code-log-name* ".zemacs-setup.log")

;;; ---------------------------------------------------------------------------
;;; Paths
;;;
;;; Every one of these is derived and none is remembered. A curriculum you
;;; rename is a curriculum whose programs are found under the new name, and a
;;; stale table of "which venv belongs to which file" is a bug that cannot be
;;; written if there is no table.

(defun %math-code-dir (org)
  "The directory a curriculum's programs live in: beside ORG and named from it.

`~/maths/linear-algebra.org' -> `~/maths/linear-algebra/'. Built by pushing the
name onto the directory component rather than by string surgery, so it is right
for a relative path, for `~/', and for a name with a dot in it."
  (let ((p (pathname org)))
    (make-pathname :directory (append (or (pathname-directory p) '(:relative))
                                      (list (pathname-name p)))
                   :name nil :type nil :version nil :defaults p)))

(defun %math-code-venv (org)
  "ORG's virtual environment directory."
  (merge-pathnames *math-code-venv-name* (%math-code-dir org)))

(defun %math-code-interpreter (org)
  "The Python inside ORG's environment.

Its existence is what \"the environment is built\" means everywhere below: a
venv is a directory with an interpreter in it, and a half-built one has no
`bin/python' to find."
  (merge-pathnames "bin/python" (%math-code-venv org)))

(defun %math-code-record (org)
  (merge-pathnames *math-code-record-name* (%math-code-venv org)))

(defun %math-code-setup-script (org)
  (merge-pathnames *math-code-setup-name* (%math-code-dir org)))

(defun %math-code-log (org)
  (merge-pathnames *math-code-log-name* (%math-code-dir org)))

(defun %math-code-run-script (org program)
  "The script that runs PROGRAM. One per program rather than one per
curriculum, so running a second program cannot rewrite the file `/bin/sh' is in
the middle of reading."
  (merge-pathnames (format nil ".zemacs-run-~a.sh" (pathname-name program))
                   (%math-code-dir org)))

(defun %math-code-curriculum-of (path)
  "The .org file whose programs directory PATH sits in, or NIL.

The inverse of `%math-code-dir', and the whole reason a program buffer needs no
state of its own: `~/maths/linear-algebra/gaussian.py' knows it belongs to
`~/maths/linear-algebra.org' because that is where it would have been put.

ponytail: the confirmation is `probe-file' and not a look inside for
`#+ZEMACS_CURRICULUM'. A `foo/bar.py' with an unrelated `foo.org' beside it
would therefore be taken for a program — which needs an org file named exactly
after the directory the Python lives in, and whose only consequence is that
`SPC m x' offers to build a venv there. Reading the preamble is the fix, and it
is a second reader for a keyword `math.lisp' already reads in the live buffer."
  (when (and path (plusp (length path)))
    (let* ((p (pathname path))
           (dir (pathname-directory p)))
      ;; A file, and one inside a directory that could be a curriculum's. The
      ;; name check is what keeps a *directory* — which is what
      ;; `buffer-file-name' answers in dired — from being read as a program.
      (when (and (pathname-name p) dir (rest dir) (stringp (car (last dir))))
        (probe-file (make-pathname :directory (butlast dir)
                                   :name (car (last dir))
                                   :type "org" :version nil :defaults p))))))

;;; ---------------------------------------------------------------------------
;;; Files
;;;
;;; Both of these answer rather than signal: everything below runs on a
;;; keystroke, and a permission error on a directory is a sentence in the status
;;; line, not a backtrace through the command loop.

(defun %math-code-read-lines (path)
  "PATH's non-blank lines, or NIL when it is not there or cannot be read.
`%math-trim' is `math.lisp''s, which loads first — a second whitespace rule
that differed slightly from that one is a bug you find in six months."
  (handler-case
      (when (probe-file path)
        (with-open-file (in path :direction :input :external-format :utf-8)
          (loop for line = (read-line in nil nil)
                while line
                for trimmed = (%math-trim line)
                when (plusp (length trimmed)) collect trimmed)))
    (error () nil)))

(defun %math-code-write (path text)
  "Write TEXT to PATH, making its directories. T, or NIL having said why."
  (handler-case
      (progn
        (ensure-directories-exist path)
        (with-open-file (out path :direction :output
                                  :if-exists :supersede
                                  :if-does-not-exist :create
                                  :external-format :utf-8)
          (write-string (or text "") out))
        t)
    (error (e)
      (message (format nil "math: cannot write ~a — ~a" (file-namestring path) e))
      nil)))

(defun %math-code-quote (string)
  "STRING as one word of `/bin/sh'.

Single quotes, which is the only quoting in the shell with no interior syntax
at all: a space, a `$', a backtick and a newline all survive it, and the one
character that does not — a quote — is closed, escaped and reopened. Every path
this file puts into a script goes through here, which is why a curriculum in
`~/My Documents' builds."
  (with-output-to-string (s)
    (write-char #\' s)
    (loop for c across string
          do (if (char= c #\')
                 (write-string "'\\''" s)
                 (write-char c s)))
    (write-char #\' s)))

;;; ---------------------------------------------------------------------------
;;; Packages
;;;
;;; What the environment should have, what it has, and the difference.

(defun math-code-packages (problem)
  "The packages PROBLEM's environment needs: the defaults, then its own.

Defaults first and duplicates dropped case-insensitively, so a problem that
lists numpy itself does not ask for it twice. PyPI treats `Foo_Bar' and
`foo-bar' as one name and this does not; a curriculum that spells one package
two ways gets it installed twice, which pip makes a no-op."
  (let ((out nil))
    (dolist (p (append *math-code-packages*
                       (and problem (getf problem :packages))))
      (pushnew p out :test #'string-equal))
    (nreverse out)))

(defun math-code-installed (org)
  "The packages recorded as installed in ORG's environment.

Read from a file the setup script appends to *after* pip succeeded, so a failed
install is not recorded and is retried. It is a record of what was asked for and
not an inventory: uninstalling something by hand behind our back leaves the
record standing, and the way to fix that is to delete `.venv'."
  (%math-code-read-lines (%math-code-record org)))

(defun math-code-missing (org packages)
  "The members of PACKAGES that ORG's environment does not have."
  (let ((have (math-code-installed org)))
    (remove-if (lambda (p) (member p have :test #'string-equal)) packages)))

(defun math-code-ready-p (org &optional packages)
  "True when ORG's environment has an interpreter and every one of PACKAGES."
  (and (probe-file (%math-code-interpreter org))
       (null (math-code-missing org (or packages *math-code-packages*)))
       t))

;;; ---------------------------------------------------------------------------
;;; Building the environment
;;;
;;; A shell script and a child that nothing waits for. The script is left on
;;; disk deliberately: it is the honest answer to "what did it actually do", it
;;; can be run by hand from any shell when something goes wrong, and it is one
;;; more thing this file does not have to explain in a status line.

(defun math-code-setup-text (org packages)
  "The script that builds ORG's environment and installs PACKAGES.

`set -e' and the ordering are the whole design. The log redirect comes first, so
even a failure to find python3 is written down; `set -e' means the record at the
end is reached only when pip returned success; and pip is run as `python -m pip'
rather than as the `pip' script, because a venv in a deep enough directory
produces a `pip' whose `#!' line is past the kernel's limit."
  (let* ((q #'%math-code-quote)
         (venv (funcall q (namestring (%math-code-venv org))))
         (python (funcall q (namestring (%math-code-interpreter org))))
         ;; No packages is not an error and must not become one: `pip install'
         ;; with no arguments fails, and `set -e' would then throw away a venv
         ;; that had just been built correctly.
         (install (if packages
                      (format nil "~a -m pip install --disable-pip-version-check~{ ~a~}
printf '%s\\n'~{ ~a~} >> ~a
"
                              python (mapcar q packages) (mapcar q packages)
                              (funcall q (namestring (%math-code-record org))))
                      "")))
    (format nil "#!/bin/sh
# Written by zemacs (runtime/modes/math-code.lisp) for
#   ~a
# and rewritten on every environment build. Nothing reads it back: it is here so
# that \"what did the editor run\" has an answer you can run yourself.
exec >~a 2>&1
set -e
echo 'zemacs: building' ~a
~a -m venv ~a
~aecho 'zemacs: environment ready'
"
            (namestring (pathname org))
            (funcall q (namestring (%math-code-log org)))
            venv
            (funcall q *math-code-python*)
            venv
            install)))

(defvar *math-code-build* nil
  "The environment build in flight, as a plist, or NIL:

    :process    the `ext:external-process' handle
    :org        whose environment is being built
    :packages   what it was asked to install
    :started    `get-internal-real-time' when it was

One at a time and not a queue: two pips writing into one venv is a broken venv,
and the second request is nearly always the same request.")

(defvar *math-code-no-python-warned* nil
  "Whether \"there is no python3\" has been said. Once per session — it is a
real limitation and does not become truer by being repeated on every keystroke
that would have used it.")

(defun %math-code-elapsed (build)
  (round (- (get-internal-real-time) (getf build :started))
         internal-time-units-per-second))

(defun math-code-build-poll ()
  "Notice that the environment build has finished, and say so.

On `*point-moved-functions*'. There is no timer in this editor, and the moment
the answer becomes interesting is the moment the user does something — so this
costs one read of a special variable per cursor movement while nothing is
building, and one `waitpid' that does not block while something is.

The verdict comes from the filesystem rather than from an exit status: the
question everything else here asks is `math-code-ready-p', and answering it a
second way is how two answers come to disagree."
  (let ((build *math-code-build*))
    (when build
      (let ((done (handler-case
                      (not (eq :running (ext:external-process-wait
                                         (getf build :process) nil)))
                    ;; A handle we can no longer ask about is a build we can no
                    ;; longer follow; treating it as finished lets the check
                    ;; below give the real answer.
                    (serious-condition () t))))
        (when done
          (let ((org (getf build :org))
                (packages (getf build :packages)))
            (setf *math-code-build* nil)
            (message
             (if (math-code-ready-p org packages)
                 (format nil "math: environment ready — ~{~a~^ ~}" packages)
                 (format nil "math: environment build failed — see ~a"
                         (namestring (%math-code-log org)))))))))))

(defun %math-code-build (org packages)
  "Start building ORG's environment for PACKAGES, unless one is already running.
Answers T when a child was started."
  (cond
    ((null (executable-find *math-code-python*))
     ;; Said once, and nothing else is touched: no directory is made and no
     ;; script is written for an environment that cannot be built.
     (unless *math-code-no-python-warned*
       (setf *math-code-no-python-warned* t)
       (message (format nil "math: no ~a on $PATH — install Python 3 to run programs"
                        *math-code-python*)))
     nil)
    (*math-code-build*
     (message (format nil "math: still building the environment (~ds)"
                      (%math-code-elapsed *math-code-build*)))
     nil)
    ;; The script is written before the fork, and a failure to write it is a
    ;; failure to build — reported by `%math-code-write' itself.
    ((null (%math-code-write (%math-code-setup-script org)
                             (math-code-setup-text org packages)))
     nil)
    (t
     (handler-case
         (multiple-value-bind (stream code process)
             ;; `:output nil' is the null device, so there is no pipe to fill
             ;; and deadlock on — and the script's own redirect on its first
             ;; line means the log is complete even where that is read as
             ;; "inherit". Orphaning the child on quit is fine, which is why
             ;; there is no cleanup here: it finishes the venv either way, and
             ;; the next session finds it built.
             (ext:run-program "/bin/sh"
                              (list (namestring (%math-code-setup-script org)))
                              :input nil :output nil :error nil :wait nil)
           (declare (ignore stream code))
           (setf *math-code-build* (list :process process
                                         :org org
                                         :packages packages
                                         :started (get-internal-real-time)))
           (message (format nil "math: building the environment — ~{~a~^ ~} · ~a"
                            packages (namestring (%math-code-log org))))
           t)
       (serious-condition (e)
         (message (format nil "math: cannot start the environment build — ~a" e))
         nil)))))

(defun %math-code-ensure (org packages)
  "Make sure ORG's environment will have PACKAGES, building if it will not.
Answers T when it is ready *now*, which is the only answer a caller can act on."
  (cond ((math-code-ready-p org packages) t)
        (t (%math-code-build org packages) nil)))

;;; ---------------------------------------------------------------------------
;;; Finding the program
;;;
;;; One function decides what "the program here" means and says why when there
;;; is none, so every command below is the same three lines and no two of them
;;; word the same refusal differently.

(defun %math-code-tangle-name (block)
  "BLOCK's `:tangle' filename when it names one this file may write, or NIL.

Quiet, because it is also asked once per problem when a program is looked up by
name. `%math-code-target' is the version that says why."
  (let ((tangle (getf block :tangle)))
    (when (and (stringp tangle)
               (plusp (length tangle))
               (not (string-equal tangle "no"))
               (not (string-equal tangle "yes"))
               ;; A curriculum is generated text, and generated text may not
               ;; name `/etc/crontab' or climb out with `..'. Everything a
               ;; problem writes is under the directory named after its file.
               (char/= (char tangle 0) #\/)
               (char/= (char tangle 0) #\~)
               (not (search ".." tangle)))
      tangle)))

(defun %math-code-target (org block)
  "The file BLOCK tangles to under ORG's directory, or NIL having said why.

`:tangle' arrives verbatim from `math-src-block', which is what makes the
refusals tellable apart: absent (a block nobody meant to write out), the string
`no' (a block that says so), `yes' (org's \"name it after the file\", for which
this format defines no name), and a path that leaves the curriculum."
  (let ((tangle (getf block :tangle))
        (name (%math-code-tangle-name block)))
    (cond (name (merge-pathnames name (%math-code-dir org)))
          ((or (null tangle) (zerop (length tangle)))
           (message "math: that block has no :tangle filename") nil)
          ((string-equal tangle "no")
           (message "math: that block is :tangle no") nil)
          ((string-equal tangle "yes")
           (message "math: :tangle needs a filename, not `yes'") nil)
          (t (message (format nil "math: refusing to tangle outside the curriculum: ~a"
                              tangle))
             nil))))

(defun %math-code-here ()
  "What the live buffer says the program is, as a plist, or NIL having said why:

    :org       the curriculum's pathname
    :problem   its problem plist, or NIL when read from the program's buffer
    :block     that problem's source block, or NIL likewise
    :program   the file the block tangles to

Two buffers count. In a *curriculum*, the problem under point decides — so the
gesture is the one `SPC m n' already leaves you in position for. In a *program*
it is the buffer itself, and there is no problem, because a file knows which
curriculum it belongs to (see `%math-code-curriculum-of') without knowing which
heading produced it.

Every refusal is worded here rather than by the callers, so the sentence a
learner reads is about the thing that was actually wrong."
  (let ((file (buffer-file-name)))
    (cond
      ((or (null file) (zerop (length file)))
       (message "math: save this buffer to a file first")
       nil)
      ;; A program, recognised by where it lives.
      ((let ((org (%math-code-curriculum-of file)))
         (when org (list :org org :problem nil :block nil
                         :program (pathname file)))))
      ((not (math-curriculum-p))
       (message "math: not a curriculum")
       nil)
      (t
       (let ((problem (math-problem-at-point)))
         (cond
           ((null problem)
            (message "math: no problem at point")
            nil)
           (t
            (let ((block (math-src-block problem)))
              (cond
                ((null block)
                 (message "math: that problem is written work — there is no program")
                 nil)
                (t
                 (let ((target (%math-code-target file block)))
                   (when target
                     (list :org (pathname file) :problem problem
                           :block block :program target)))))))))))))

(defun %math-code-block-of (org problem)
  "PROBLEM's source block, read in ORG's buffer.

`math-src-block' reads the *live* buffer, so asking about a problem in a
curriculum you are not looking at needs the switch. A no-op when it already is
the live buffer, which is the common case."
  (let ((name (file-namestring org)))
    (if (equal (buffer-file-name) (namestring org))
        (math-src-block problem)
        (when (member name (buffer-names) :test #'string=)
          (with-current-buffer name (math-src-block problem))))))

(defun %math-code-problem-for (org program)
  "The problem in ORG whose block tangles to PROGRAM, or NIL.

Wanted only by `math-code-reset' called from a program's own buffer, where
there is no point in a curriculum to read a problem from. The curriculum has to
be *open*: its text is what `math-problems' reads, and a second org parser that
worked on a file rather than on a buffer is not worth one command."
  (let ((name (file-namestring org))
        (want (file-namestring program)))
    (when (member name (buffer-names) :test #'string=)
      (with-current-buffer name
        (find-if (lambda (p)
                   (let ((block (math-src-block p)))
                     (and block
                          (equal want (%math-code-tangle-name block)))))
                 (math-problems))))))

;;; ---------------------------------------------------------------------------
;;; Windows
;;;
;;; `%repl-show' in `repl.lisp' is the worked example, and this is the same
;;; gesture with the split turned ninety degrees: `split-window-right' rather
;;; than `-below', because what was asked for is the curriculum on one side and
;;; the program on the other, and because a page of prose and a page of code
;;; each want a column rather than half the rows.

(defun %math-code-window (name)
  "The window of the focused frame showing the buffer called NAME, or NIL."
  (find name (window-list) :key #'second :test #'string=))

(defun %math-code-show (path)
  "Put PATH on screen beside what is there now, and leave the cursor in it.

Two cases and a line each: it is already on screen, so go there; it is not, so
split and open it. Focus *stays* in the new pane, which is where `%repl-show'
deliberately does the opposite — a transcript is something you glance at while
working elsewhere, and a program is the thing you came here to write.

`split-window-right' focuses the new pane, so `find-file' lands in it. Both
travel the same queue the app drains in order, which is the whole trick and is
documented at `%repl-show'."
  (let ((win (%math-code-window (file-namestring path))))
    (cond (win (select-window (first win)))
          (t (split-window-right)
             (find-file (namestring path))))))

(defun %math-code-save (program)
  "Save PROGRAM if a buffer of it has unsaved changes.

Running the file while the buffer holding your last edit has not reached it is
the oldest wasted debugging session there is, and it is worth the buffer switch
to close: `save-file' saves the *live* buffer, so reaching a program you are not
looking at means going there and coming back."
  (let ((name (file-namestring program)))
    (cond ((equal (buffer-file-name) (namestring program))
           (when (buffer-modified-p) (save-file)))
          ((member name (buffer-names) :test #'string=)
           (with-current-buffer name
             (when (buffer-modified-p) (save-file)))))))

;;; ---------------------------------------------------------------------------
;;; Running
;;;
;;; Two functions that produce *text*, and one that hands the text to the
;;; editor. Separated so the whole thing can be read — and tested — without
;;; forking anything, which is what `%ai-verb' is for in `ai.lisp' and for the
;;; same reason: this file's entire contribution to the terminal is one line.

(defun math-code-run-text (org program)
  "The script that runs PROGRAM in ORG's environment.

`cd' first, so the program's own `open(\"data.csv\")' means the file beside it
rather than whatever directory the editor happened to be started from — which
is what a `terminal-run' spawned from a window showing something else would give
it. `exec' after, so there is one process to interrupt rather than a shell
holding a child that `C-c' would leave running."
  (let ((q #'%math-code-quote))
    (format nil "#!/bin/sh
# Written by zemacs (runtime/modes/math-code.lisp). Run it yourself to see
# exactly what `SPC m x' does.
cd ~a || exit 1
exec ~a ~a
"
            (funcall q (namestring (%math-code-dir org)))
            (funcall q (namestring (%math-code-interpreter org)))
            (funcall q (namestring program)))))

(defun math-code-run-verb (org program)
  "The editor verb that runs PROGRAM: `run:NAME:/bin/sh SCRIPT'.

NAME is the program's own, so the buffer is `*gaussian*' and the session
switcher says whose output you are looking at.

ponytail: `terminal-run' splits its command line on whitespace, so a curriculum
whose *path* contains a space cannot be run — the script's own quoting handles
everything inside it, but not the one argument naming the script. The upgrade is
the `Term { verb, args }' command `crates/app/src/term.rs' already names as the
fix for the same limitation in the AI harnesses; both wait on the same line.

`rerun:' rather than `run:', which is the difference between running a program
and starting a harness. `run:' opens a session per press — right for an agent,
where two side by side is the point — and that piled `*gaussian*<2>', `<3>' into
the switcher, every one of them a finished child, because a program is a thing
you run *again*: edit, run, read, edit. `rerun:' replaces the session of that
name and keeps its buffer, so the window showing it and its place in the
switcher survive. `C-M-r' still re-runs in place from inside the terminal."
  (format nil "rerun:~a:/bin/sh ~a"
          (pathname-name program)
          (namestring (%math-code-run-script org program))))

;;; ---------------------------------------------------------------------------
;;; The commands

(defun math-code-edit ()
  "Open the program for the problem at point, beside the curriculum.

The one gesture this file exists for. It writes the template the *first* time
and never again, starts the environment building when there is not one, splits
the window and leaves you in the program — and skips each of those when it has
already happened, so pressing it twice is the same as pressing it once."
  (math-code-build-poll)
  (let ((here (%math-code-here)))
    (when here
      (let* ((org (getf here :org))
             (problem (getf here :problem))
             (program (getf here :program))
             (fresh (null (probe-file program))))
        ;; Only the curriculum carries the template, and only a file that is not
        ;; there is written. Both halves matter: this is the keystroke you press
        ;; to *look* at a problem, and it must never cost you an afternoon.
        (cond
          ((and fresh problem (getf here :block))
           ;; A write that failed has already said why, and saying anything
           ;; after it would put the real reason off the status line.
           (unless (%math-code-write program (getf (getf here :block) :body))
             (return-from math-code-edit nil)))
          (fresh
           (message (format nil "math: ~a is not there — open it from the curriculum"
                            (file-namestring program)))
           (return-from math-code-edit nil)))
        (%math-code-ensure org (math-code-packages problem))
        (%math-code-show program)
        (message (format nil "~a~:[~; · tangled~] · SPC m x runs it"
                         (file-namestring program) fresh))))))

(defun math-code-run ()
  "Run the program for the problem at point, in a terminal of its own.

The output goes to a real PTY, so the program may prompt, print as it goes, be
interrupted with `C-c' and open a window; `C-M-r' runs it again in the same
buffer and `C-M-w' closes it, which are `ai.lisp''s session keys and work here
unchanged.

Nothing looks at what comes out. This shows you your program's output; whether
it is the *right* output is yours to decide."
  (math-code-build-poll)
  (let ((here (%math-code-here)))
    (when here
      (let ((org (getf here :org))
            (program (getf here :program)))
        ;; Not there yet? Write it, and carry on into the run below.
        ;;
        ;; This used to refuse and name another key. "Run this" is a complete
        ;; instruction, and answering it with "press SPC m e first" is the
        ;; editor asking you to do its bookkeeping — the template is sitting in
        ;; the curriculum, the directory is derivable, and nothing about the
        ;; request was ambiguous. It also made the two keys *ordered*, which is
        ;; the half nobody remembers a week later.
        ;;
        ;; Only from the curriculum, where a `:block' is in reach; from the
        ;; program's own buffer a missing file means it was deleted out from
        ;; under the editor and there is nothing to write it from. Falling
        ;; through rather than calling this function again, because the write
        ;; leaving the file still absent would then be an unbounded recursion
        ;; instead of the message below.
        (when (and (null (probe-file program)) (getf here :block))
          (when (%math-code-write program (getf (getf here :block) :body))
            (%math-code-show program)))
        (cond
          ((null (probe-file program))
           (message (format nil "math: no ~a — open it from the curriculum"
                            (file-namestring program))))
          ;; A program run against half an environment fails with an ImportError
          ;; that blames the program. `%math-code-ensure' has already said
          ;; whether it is building or cannot.
          ;;
          ;; ponytail: from the *program's* buffer there is no problem to read
          ;; `:ZEMACS_PACKAGES:' off, so only the defaults are checked — the
          ;; problem's own went in when `math-code-edit' opened it, and finding
          ;; the heading again would mean a scan of the curriculum, with the
          ;; buffer switch that needs, on the key pressed most.
          ((not (%math-code-ensure org (math-code-packages (getf here :problem))))
           nil)
          (t
           (%math-code-save program)
           (when (%math-code-write (%math-code-run-script org program)
                                   (math-code-run-text org program))
             (terminal (math-code-run-verb org program)))))))))

(defun math-code-env ()
  "Build or top up this curriculum's environment, and say where it stands.

The command for when something went wrong: a package added to the problem after
you opened it, or a build that failed and you want it again. Never needed on the
happy path, because `math-code-edit' does it."
  (math-code-build-poll)
  (let ((here (%math-code-here)))
    (when here
      (let* ((org (getf here :org))
             (packages (math-code-packages (getf here :problem))))
        (cond
          (*math-code-build*
           (message (format nil "math: building the environment (~ds) — ~a"
                            (%math-code-elapsed *math-code-build*)
                            (namestring (%math-code-log org)))))
          ((math-code-ready-p org packages)
           (message (format nil "math: environment ready — ~{~a~^ ~} · ~a"
                            (math-code-installed org)
                            (namestring (%math-code-venv org)))))
          (t
           ;; Asked for by name, so the "no python3" sentence is worth saying
           ;; again even where it has been said before.
           (setf *math-code-no-python-warned* nil)
           (%math-code-build org packages)))))))

(defun math-code-reset ()
  "Put the curriculum's template back, discarding what is in the program.

Asks first, and the answer is one word. When the program is open — which it
almost always is, since this is the command you reach for while looking at it —
the *buffer* is rewritten in a single `replace-region', so `u' undoes the whole
reset in one step. When it is not open the file is overwritten and there is
nothing to undo, which is the case the question is really about."
  (let ((here (%math-code-here)))
    (when here
      (let* ((org (getf here :org))
             (program (getf here :program))
             (problem (or (getf here :problem)
                          (%math-code-problem-for org program)))
             (block (or (getf here :block)
                        (and problem (%math-code-block-of org problem))))
             (body (getf block :body))
             (name (file-namestring program)))
        (cond
          ((null body)
           (message (format nil "math: no template for ~a — is the curriculum open?"
                            name)))
          (t
           (completing-read (format nil "Reset ~a to the template? " name)
                            '("no" "yes")
             (lambda (pick)
               (when (and pick (string= pick "yes"))
                 (cond
                   ((member name (buffer-names) :test #'string=)
                    (with-current-buffer name
                      (replace-region (point-min) (point-max) body))
                    (message (format nil "~a reset to the template — u undoes it"
                                     name)))
                   ((%math-code-write program body)
                    (message (format nil "~a rewritten from the template"
                                     name)))))))))))))

(defun math-code-back ()
  "Go back to the curriculum this program belongs to.

Its window when it has one — where point is already sitting on the problem you
left — and otherwise open it. Deliberately does not hunt for the heading: the
curriculum remembers where you were, and moving point would take that away."
  (let ((org (%math-code-curriculum-of (buffer-file-name))))
    (cond
      ((null org) (message "math: this is not a curriculum program"))
      (t (let ((win (%math-code-window (file-namestring org))))
           (if win
               (select-window (first win))
               (find-file (namestring org))))))))

;;; ---------------------------------------------------------------------------
;;; The program's minor mode
;;;
;;; A curriculum's programs are ordinary Python files and must stay ordinary —
;;; the highlighter, the LSP client and everything else in this editor already
;;; know what a `.py' is. So this is a *minor* mode carrying four keys, switched
;;; on for the files that live in a curriculum's directory and off everywhere
;;; else: the same shape and the same argument as `math-curriculum' in
;;; `math.lisp'.
;;;
;;; Switching it on needs a hook, and `python-mode' had none — the modes in
;;; `library.lisp' have empty bodies. Rather than put one there, which would
;;; make the standard library know about a maths curriculum, this re-declares
;;; the mode with a body that runs a *list*, exactly as `org-modern.lisp' does
;;; for `*org-mode-functions*'. That re-declaration is also the fragile part:
;;; `define-derived-mode' replaces a mode's body, so the last declaration in the
;;; runtime wins, and a second file wanting a `python-mode' body must push onto
;;; the list below rather than declare the mode again.

(defvar *python-mode-functions* nil
  "Functions called with no arguments when a buffer enters `python-mode'.")

(define-derived-mode python-mode prog-mode
  (dolist (f *python-mode-functions*) (ignore-errors (funcall f))))

(define-minor-mode math-code
  "Keys for a curriculum's program: `SPC m x' runs it, `SPC m g' goes back to
the curriculum, `SPC m v' says how the environment is doing and `SPC m R' puts
the template back.")

(defun math-code-maybe ()
  "Turn `math-code' on in a buffer that is a curriculum's program.

Guarded by `minor-mode-p' because the mode command is a toggle and the hook runs
on every entry into python-mode — without it, re-entering would switch the keys
back off. The message is last so that it is the line left on screen: the toggle
ends by announcing itself, and `math-code on' is not what a learner needs to
read."
  (when (and (not (minor-mode-p 'math-code))
             (%math-code-curriculum-of (buffer-file-name)))
    (math-code)
    (message (format nil "~a — SPC m x runs it, SPC m g goes back"
                     (buffer-name)))))

(pushnew 'math-code-maybe *python-mode-functions*)

;;; The build poller, on the hook `modes.lisp' runs after every cursor movement.
;;; DEFVAR before the PUSHNEW as everywhere else that joins a hook list: a build
;;; whose `*runtime-dir*' never found that file must get an empty list here
;;; rather than an unbound variable.
(defvar *point-moved-functions* nil)
(pushnew 'math-code-build-poll *point-moved-functions*)

;;; ---------------------------------------------------------------------------
;;; Keys
;;;
;;; In the curriculum, beside `math.lisp''s motions (`SPC m n/p/t/g/s') and
;;; `math-written.lisp''s capture (`SPC m r/w'). In the program, the ones that
;;; mean something there.
;;;
;;; `SPC m x' is deliberately the same key in both halves of the split: it is
;;; the one pressed most, and having to remember which side you are on in order
;;; to run your program is exactly the ceremony this file is against. `x' rather
;;; than the obvious `r' because `r' is *capture* in a curriculum, and one key
;;; meaning two things in one file is worse than a second-choice mnemonic — `x'
;;; is execute, as it is in `M-x'.

(define-key "math-curriculum" "SPC m e" "math-code-edit")
(define-key "math-curriculum" "SPC m x" "math-code-run")
(define-key "math-curriculum" "SPC m v" "math-code-env")
(define-key "math-curriculum" "SPC m R" "math-code-reset")

(define-key "math-code" "SPC m x" "math-code-run")
(define-key "math-code" "SPC m g" "math-code-back")
(define-key "math-code" "SPC m v" "math-code-env")
(define-key "math-code" "SPC m R" "math-code-reset")

;;; `math-code-build-poll' is machinery rather than a verb: it is on a hook and
;;; running it by hand does nothing you can see. `*hidden-commands*' is
;;; `init.lisp''s existing answer to exactly that, and is why this is declared
;;; here rather than renamed with a `%' it would then share with the internals.
(when (boundp '*hidden-commands*)
  (pushnew "math-code-build-poll" *hidden-commands* :test #'string=))
