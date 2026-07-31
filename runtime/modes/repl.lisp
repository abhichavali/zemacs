;;;; A REPL, in the image that is running the editor.
;;;;
;;;; The design question first, because everything below follows from it.
;;;;
;;;; "Open a REPL" could mean spawning a second ECL and talking to it down a
;;;; pipe, the way SLIME talks to an inferior Lisp. It does not mean that here,
;;;; and it should not: *this editor already is an ECL image*. `init.lisp' was
;;;; LOADed into it, every key binding calls into it, and `C-c' has always
;;;; evaluated the live buffer there. A REPL in a child process would be a
;;;; different image — `(set-font-size 30)' typed into it would change nothing,
;;;; `(defun save-file ...)' would advise nobody, and the one property that
;;;; makes a Lisp machine a Lisp machine, that the thing you are configuring is
;;;; the thing you are talking to, would be gone. So: the live image, the same
;;;; one `M-x' and the scratch buffer already reach.
;;;;
;;;; What that costs, and what is done about it:
;;;;
;;;; - A signalled condition would otherwise unwind through whatever called us.
;;;;   Every evaluation here is wrapped in `handler-case' on `serious-condition'
;;;;   — wider than `error' on purpose, so a runaway recursion comes back as a
;;;;   line in the transcript rather than as a dead image — and printing the
;;;;   condition is wrapped again, since a condition's own printer can signal.
;;;; - `(loop)' is *not* contained and cannot be. The Lisp thread evaluates one
;;;;   queued form at a time and nothing can preempt it, so a form that never
;;;;   returns costs you the image, though not the editor: input and redisplay
;;;;   carry on, and files can still be saved. That is a ceiling of the whole
;;;;   design, listed as one in docs/boundary.org, and a child process is the
;;;;   only thing that would fix it. Not worth the rest.
;;;;
;;;; The transcript is an ordinary `.lisp' file, so it highlights, survives a
;;;; restart, and — because every line the REPL adds is a *comment* — LOADs.
;;;; Evaluating the buffer replays the session.

(in-package :zemacs)

(defparameter *repl-file*
  (merge-pathnames ".config/zemacs/repl.lisp" (user-homedir-pathname))
  "Where the transcript lives. A real file for the same reason the scratch
buffer is one: `find-file' is the only thing that can put the editor in a
*different* buffer, and there is no way to make one with nothing behind it.")

(defparameter *repl-buffer* "repl.lisp"
  "What that file is called once it is open — buffers are named after the last
component of their path.")

(defun %repl-header ()
  (format nil ";;; *repl* — ~a ~a
;;;
;;; A transcript, not a prompt. Every line this buffer gains is a comment, so
;;; the whole file is still Lisp and evaluating it replays the session.
;;;
;;;   C-c C-e   evaluate the expression before point
;;;   C-c C-c   evaluate the top-level form point is in
;;;   C-c C-r   evaluate the selection
;;;   C-c C-k   evaluate the whole buffer
;;;   C-c C-z   come back here
;;;
;;; Forms are evaluated in the image that is *running the editor*, so a DEFUN
;;; here redefines the editor's own behaviour on the spot. That is the point.

"
          (lisp-implementation-type)
          (lisp-implementation-version)))

(defun %repl-ensure-file ()
  (ensure-directories-exist *repl-file*)
  (unless (probe-file *repl-file*)
    (with-open-file (out *repl-file*
                         :direction :output
                         :if-does-not-exist :create
                         :external-format :utf-8)
      (write-string (%repl-header) out))))

(defun lisp-repl ()
  "Show the REPL transcript, creating it the first time."
  (handler-case (progn (%repl-ensure-file) (find-file (namestring *repl-file*)))
    (error (e) (message (format nil "repl: ~a" e)))))

(defun %repl-write (text)
  "Append TEXT to the transcript: into the buffer when it is open, into the
file when it is not.

Deliberately never opens it. An eval from a source buffer must not take the
window you are working in — `C-c C-z' is how you ask for that — and appending
to the file keeps the record complete either way, so the transcript is all
there the first time you look at it."
  (if (member *repl-buffer* (buffer-names) :test #'string=)
      ;; Two locks and a switch: the REPL is genuinely the live buffer for the
      ;; length of one insert, which is short enough that a keystroke landing
      ;; in it is a curiosity rather than a hazard. `insert-at' leaves point on
      ;; the new end, which is where a REPL wants it.
      (with-current-buffer *repl-buffer* (insert-at (point-max) text))
      (handler-case
          (progn
            (%repl-ensure-file)
            (with-open-file (out *repl-file*
                                 :direction :output
                                 :if-exists :append
                                 :external-format :utf-8)
              (write-string text out)))
        (error (e) (message (format nil "repl: ~a" e))))))

;;; ---------------------------------------------------------------------------
;;; Evaluating

(defun %repl-print (value)
  "VALUE as the REPL should show it. `~S' so \"3\" and 3 are distinguishable,
under the same print limits `eval-string' uses — a circular or enormous
structure must not become a transcript nobody can read. Printing is itself
guarded: a `print-object' method is user code and can signal like any other."
  (handler-case
      (let ((*print-length* 32) (*print-level* 4) (*print-circle* t))
        (format nil "~s" value))
    (serious-condition () "#<unprintable>")))

(defun %condition-string (e)
  "E's report, or a stand-in. `~a' on a condition runs its reporter, which is
user code, so this is the one place that has to survive its own error message."
  (handler-case (format nil "~a" e)
    (serious-condition () "#<unprintable condition>")))

(defun %one-line (s)
  "S with its newlines flattened. The status line is one line and a transcript
comment is one line; a multi-line value is still readable run together, and
`*print-length*' has already capped how much of it there is."
  (substitute #\Space #\Newline s))

(defun %eval-source (source)
  "Read and evaluate every form in SOURCE, in the live image. Answers the last
value and NIL, or NIL and the condition's report.

`serious-condition' rather than `error': a stack overflow is not an `error' in
Common Lisp, and it is precisely the one a REPL produces by accident."
  (handler-case
      (let ((*package* (find-package :zemacs))
            (value nil))
        (with-input-from-string (in source)
          (loop for form = (read in nil '%eof)
                until (eq form '%eof)
                do (setf value (eval form))))
        (values value nil))
    (serious-condition (e) (values nil (%condition-string e)))))

(defun %repl-entry (source value error)
  "One transcript entry: the source as written, then its value as a comment."
  (format nil "~%~a~%~a~%"
          (string-trim '(#\Space #\Tab #\Newline #\Return) source)
          (if error
              (format nil ";!! ~a" (%one-line error))
              (format nil ";=> ~a" (%one-line (%repl-print value))))))

(defun %repl-eval (source)
  "Evaluate SOURCE, record it in the transcript, and echo its value."
  (if (or (null source)
          (zerop (length (string-trim '(#\Space #\Tab #\Newline #\Return) source))))
      (message "nothing to evaluate")
      (multiple-value-bind (value error) (%eval-source source)
        (%repl-write (%repl-entry source value error))
        (message (if error
                     (format nil "lisp error: ~a" (%one-line error))
                     (%one-line (%repl-print value)))))))

;;; ---------------------------------------------------------------------------
;;; The commands
;;;
;;; SLIME's names, because those are the ones in everybody's fingers. What each
;;; one *evaluates* is worked out by the scanner in `lisp-mode.lisp', so these
;;; are four ways of picking a slice of text and one way of running it.

(defun lisp-eval-last-sexp ()
  "Evaluate the expression that ends at point."
  (%with-lisp-scan (text classes here)
    (let* ((at (min (length text) (1+ here)))   ; point sits *on* a character
           (start (%sexp-start text classes at)))
      (if start
          (%repl-eval (subseq text start (%skip-blanks-back text classes at)))
          (message "no complete expression before point")))))

(defun lisp-eval-defun ()
  "Evaluate the top-level form point is in — SLIME's `C-c C-c'."
  (%with-lisp-scan (text classes here)
    (let ((top (%toplevel-form text classes (min (length text) (1+ here)))))
      (if top
          (%repl-eval (subseq text (car top) (cdr top)))
          (message "no top-level form here")))))

(defun lisp-eval-region ()
  "Evaluate the selection, and drop back to Normal state as `eval-region' does."
  (let ((text (region-text)))
    (if text
        (progn (%repl-eval text) (set-evil-state "normal"))
        (message "no selection"))))

(defun lisp-eval-buffer ()
  "Evaluate the whole buffer."
  (%repl-eval (buffer-string)))

;;; ---------------------------------------------------------------------------
;;; Keys
;;;
;;; The `C-c' family, which needs a word of explanation. `init.lisp' binds bare
;;; `C-c' to `eval-dwim' in every state, and an exact binding used to be found
;;; before any *prefix* could be — so `C-c C-e' could never be typed. Core now
;;; lets a mode-local prefix outrank a global exact binding, which is the same
;;; rule the reference already states for bindings ("a mode-local binding wins
;;; over a global one") carried through to sequences. The effect is contained:
;;; `C-c' is a prefix in a Lisp buffer and unchanged everywhere else, and
;;; which-key shows the family the moment you press it.
;;;
;;; `C-x C-e' and `C-M-x' are here as well, because they are what Emacs itself
;;; binds and half of the muscle memory is that half.

(define-mode-key 'lisp-mode "C-c C-e" "lisp-eval-last-sexp")
(define-mode-key 'lisp-mode "C-c C-c" "lisp-eval-defun")
(define-mode-key 'lisp-mode "C-c C-r" "lisp-eval-region")
(define-mode-key 'lisp-mode "C-c C-k" "lisp-eval-buffer")
(define-mode-key 'lisp-mode "C-c C-z" "lisp-repl")
(define-mode-key 'lisp-mode "C-x C-e" "lisp-eval-last-sexp")
(define-mode-key 'lisp-mode "C-M-x"   "lisp-eval-defun")

;;; The leader spellings, so the whole thing is discoverable by holding SPC and
;;; reading what which-key puts up.
(define-mode-key 'lisp-mode "SPC m r" "lisp-repl")
(define-mode-key 'lisp-mode "SPC m d" "lisp-eval-defun")
(define-mode-key 'lisp-mode "SPC m k" "lisp-eval-buffer")

;;; `lisp-repl' from anywhere: the REPL is somewhere you go, not only something
;;; a Lisp buffer has. Spelled out with `define-key' rather than with the
;;; config's `define-leader', for the reason `library.lisp' gives — a file under
;;; runtime/modes/ has to load whether or not an init file has defined that yet.
(dolist (state '("normal" "visual" "visual-line" "visual-block"))
  (define-key state "SPC o r" "lisp-repl"))
