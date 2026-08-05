;;;; zemacs-tutor — a tutorial that marks your homework, in two panes.
;;;;
;;;; Emacs ships a tutor and it is a *file*: a page of prose that tells you to
;;;; press C-n and then trusts you. That was the right design in 1985 and it is
;;;; the wrong one here, because this editor is a Common Lisp image and an image
;;;; can run the answer. So: instruction on the left, a Lisp buffer on the
;;;; right, and a key that evaluates what you wrote and tells you whether it is
;;;; right.
;;;;
;;;;   +---------------------------+---------------------------+
;;;;   |  *tutor*                  |  *tutor-lisp*             |
;;;;   |  org-mode: the lesson     |  lisp-mode: your answer   |
;;;;   |  RET follows a link       |  SPC m c checks it        |
;;;;   |  SPC m n / SPC m p        |                           |
;;;;   +---------------------------+---------------------------+
;;;;
;;;; Three stages: Common Lisp, then this editor's own Lisp API, then a
;;;; placeholder. See the foot of the data section.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; THE LEFT PANE IS ORG, AND THAT IS THE WHOLE PRESENTATION
;;;;
;;;; The first version of this file drew its own lesson buffer: hand-rolled
;;;; rules out of box-drawing characters, sentinel comments delimiting the
;;;; answer areas, and `display' overlays making the sentinels look like
;;;; headings. It worked and it looked like a text adventure, because it was
;;;; reimplementing — badly, in about two hundred lines — a document renderer
;;;; that was already sitting next door.
;;;;
;;;; So the instruction pane is an ordinary org buffer. Not org-ish, not a mode
;;;; deriving from org: `org-mode' itself, arrived at by `set-major-mode', which
;;;; means every one of these is inherited rather than written here —
;;;;
;;;;   real headings, in real bold, at two sizes         (org-modern.lisp)
;;;;   bullets instead of stars, `=code=' drawn as code
;;;;   `[[id:x][text]]' drawn as its description alone
;;;;   an 80-column measure, centred, with no gutter     (library.lisp)
;;;;   `z a' to fold a stage or a lesson                 (org-fold.lisp)
;;;;   `RET' to follow a link                            (org-modern.lisp)
;;;;   LaTeX and inline images, if a lesson ever wants them
;;;;
;;;; — and none of them knows this feature exists. That is the same argument
;;;; `runtime/modes/math.lisp' makes at greater length for a curriculum, and
;;;; this file is deliberately shaped like it: a *schema* over ordinary org,
;;;; read with `%org-lines', `%org-line-level' and `%org-property' from
;;;; `org-modern.lisp' rather than with a parser of its own. Two features, one
;;;; editor.
;;;;
;;;; The document itself is generated from `*tutor-stages*' every time you open
;;;; the tutor, which is why it is a `create-buffer' buffer with no file behind
;;;; it — `docs/reference.org' calls that the shape for a generated view, and a
;;;; file would only be a copy of the data going stale. The *truth* about your
;;;; progress lives in `~/.zemacs.d/tutor.lisp'; what you see in the org
;;;; buffer is that truth rendered, which is why there is never a question of
;;;; the two disagreeing.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; HOW AN ANSWER IS CHECKED, AND WHY THE TWO STAGES DIFFER
;;;;
;;;; Unchanged from the first version, and the part of this file that was right
;;;; the first time.
;;;;
;;;; *Stage 1 runs in a child `ecl'.* Not in this image, and the reason is one
;;;; line in `repl.lisp': "`(loop)' is *not* contained and cannot be. The Lisp
;;;; thread evaluates one queued form at a time and nothing can preempt it." A
;;;; REPL is something you point at yourself; a tutorial is something a beginner
;;;; points at themselves by accident, on lesson nine, at the exact moment they
;;;; are least able to tell a hung image from a hard exercise. So a Stage 1
;;;; answer is written to a file, handed to the system `ecl' binary with
;;;; `ext:run-program', and given `*tutor-timeout*' seconds to come back.
;;;;
;;;; The child is also why Stage 1 can be honest about `defun'. The check does
;;;; not look at your *source*; it runs it and then asks questions of what it
;;;; produced — `(= (my-length '(a b c)) 3)' and two more calls besides. Get
;;;; there by recursion, by `loop', by `reduce' or by counting on your fingers:
;;;; all of them pass, because all of them are right.
;;;;
;;;; *Stage 2 runs in the live image*, through `%eval-source' from `repl.lisp'.
;;;; That is not a compromise made to save a fork. Stage 2's exercises are about
;;;; controlling this editor: when the lesson says `(set-font-size 30)' the text
;;;; has to get bigger, and a child process would resize nothing and be teaching
;;;; a fiction. The checks observe the editor afterwards through its own
;;;; readers, which is exactly what a plugin does.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; LESSONS ARE DATA
;;;;
;;;; `*tutor-stages*' is a list and nothing below it is a special case. A stage
;;;; is a plist with a `:lessons' list; a lesson is org prose plus `:exercises';
;;;; an exercise is a `:prompt', a `:template' to start from, a `:check' form
;;;; and a `:hint'. Adding a lesson is adding a list entry.
;;;;
;;;; A `:check' is a *predicate over the value your code produced*, with `value'
;;;; bound to it, and it may call anything your code defined. In Stage 1 it is
;;;; read and evaluated inside the child at run time — deliberately, so that a
;;;; macro you have just defined is available to the form that tests it.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; Dependencies: `org-modern.lisp' (for `%org-lines', `%org-line-level',
;;;; `%org-property' and the org-mode body itself), `modes.lisp' (for
;;;; `define-minor-mode'), `repl.lisp' (for `%eval-source', `%condition-string'
;;;; and `%repl-print') and `ai.lisp' (for `executable-find', which this file
;;;; guards with `fboundp'). It is loaded after all four in `init.lisp'.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; Where things live, and what you can turn

(defparameter *tutor-buffer* "*tutor*"
  "The instruction pane, on the left. An org buffer with no file behind it:
`create-buffer' is idempotent by name, so every render is `make it, then
rewrite it', and the document is regenerated from `*tutor-stages*' rather than
stored.")

(defparameter *tutor-lisp-buffer* "*tutor-lisp*"
  "The answer pane, on the right. An ordinary `lisp-mode' buffer, so it gets
the indenter, parinfer and the whole `C-c C-*' REPL family — you can evaluate a
form in the live image while you are working on it, which is the fastest way to
find out what your answer actually does.")

(defparameter *tutor-playground* "*tutor-scratch*"
  "The buffer Stage 2's exercises are evaluated in. Not the answer pane: an
exercise that replaces the whole buffer would replace the answer you are being
marked on. `SPC m w' shows it.")

(defparameter *tutor-progress-file*
  (zemacs-file "tutor.lisp")
  "Where you got to, as ordinary Lisp data. Beside `scratch.lisp' and
`repl.lisp', because that is where this editor keeps the things it wrote for
you and you are allowed to edit. This is the *truth* about your progress; the
marks in the org document are a rendering of it.")

(defparameter *tutor-check-file*
  (zemacs-file "tutor-check.lisp")
  "The program handed to the child `ecl', overwritten before every Stage 1
check. A file rather than a pipe on purpose: when a check goes wrong you can
run `ecl --norc --shell' on it yourself and watch it go wrong.")

(defparameter *tutor-ecl* "ecl"
  "The Common Lisp that marks Stage 1. Any ANSI implementation would do — the
harness uses nothing outside the standard.")

(defparameter *tutor-timeout* 8
  "Seconds a Stage 1 answer gets before it is killed. Generous: a cold `ecl'
starts in under a tenth of a second, so anything near this is a loop.")

(defparameter *tutor-stages* nil
  "The tutorial itself. Filled in at the foot of this file, under `The
lessons' — declared up here so the machinery in between refers to a variable
that exists, and so a config wanting a stage of its own has one name to
`push' onto and one shape to copy.")

;;; The schema. Every literal the document is written with and read back by, in
;;; one place, the way `math.lisp' keeps a curriculum's vocabulary in one place.
;;;
;;; Two properties per lesson rather than one, and the reason is worth stating:
;;; `:ID:' is *org's* and is what an `[[id:...]]' link in the Contents resolves
;;; against, so following a link is `org-open-at-point' unmodified and not a
;;; line of this file. `:ZEMACS_TUTOR_LESSON:' is ours and carries the two ids
;;; separated by a space, because an id built by joining two hyphenated names
;;; cannot be taken apart again — `tutor-cl-lambda-lists' has no seam in it.

(defparameter *tutor-keyword* "ZEMACS_TUTOR"
  "The `#+' keyword that declares a buffer the tutor. Read by
`tutor-lesson-maybe' from `*org-mode-functions*', which is how the minor mode
switches itself on.")

(defparameter *tutor-lesson-property* "ZEMACS_TUTOR_LESSON"
  "On a lesson heading: `STAGE-ID LESSON-ID'.")

(defparameter *tutor-exercise-property* "ZEMACS_TUTOR_EXERCISE"
  "On an exercise heading: `STAGE-ID LESSON-ID INDEX'.")

(defparameter *tutor-id-property* "ID"
  "Org's own, so the Contents links work in Emacs unchanged.")

(defparameter *tutor-contents-heading* "Contents"
  "The heading `tutor-contents' rewrites and jumps back to.")

(defparameter *tutor-done-mark* "✓"
  "What a passed exercise's heading gains. Display only: the pass itself lives
in the progress file, and this is re-derived from it on every render — so there
is no second place for a status to be wrong.")

(defparameter *tutor-verdict-mark* ";;tutor: "
  "What the child prints its answer behind. Your own `(format t ...)' output is
captured too and shown to you; the *last* line carrying this prefix is the
verdict, and the harness prints it after your code has finished.")

;;; ---------------------------------------------------------------------------
;;; Small string helpers
;;;
;;; Five of them, because the alternative is `search' and `position' spelled out
;;; at fifteen call sites. Nothing here is clever.

(defun %tutor-prefixp (prefix string)
  "True when STRING starts with PREFIX."
  (let ((n (length prefix)))
    (and (<= n (length string)) (string= prefix string :end2 n))))

(defun %tutor-trim (string)
  (string-trim '(#\Space #\Tab #\Newline #\Return) string))

(defun %tutor-blankp (string)
  (zerop (length (%tutor-trim string))))

(defun %tutor-lines (text)
  "TEXT split on newlines. A trailing newline does not make a last empty line,
which is the reading every caller here wants."
  (let ((out nil) (i 0) (n (length text)))
    (loop while (<= i n)
          do (let ((j (or (position #\Newline text :start i) n)))
               (push (subseq text i j) out)
               (setf i (1+ j))))
    (let ((lines (nreverse out)))
      (if (and (rest lines) (string= "" (first (last lines))))
          (butlast lines)
          lines))))

(defun %tutor-words (string)
  "STRING split on whitespace, empties dropped. What takes a property value
like `cl reader 0' apart again."
  (let ((out nil) (start nil) (n (length string)))
    (dotimes (i n)
      (let ((blank (member (char string i) '(#\Space #\Tab #\Return))))
        (cond ((and blank start) (push (subseq string start i) out) (setf start nil))
              ((and (not blank) (null start)) (setf start i)))))
    (when start (push (subseq string start) out))
    (nreverse out)))

(defun %tutor-one-line-short (text)
  "TEXT on one line and short enough for a status line. `%one-line' is
`repl.lisp''s; the truncation is this file's, because a condition report from a
child image can be a paragraph and the status line is not one."
  (let ((flat (%one-line (or text ""))))
    (if (> (length flat) 90) (concatenate 'string (subseq flat 0 90) "...") flat)))

;;; ---------------------------------------------------------------------------
;;; Walking the data
;;;
;;; Stages and lessons are found by *id*, never by position: a lesson inserted
;;; in the middle must not silently move somebody's saved place to a different
;;; lesson, and an id that no longer exists must land you somewhere sensible
;;; rather than locking you out.

(defun %tutor-stage (id)
  (find id *tutor-stages* :key (lambda (s) (getf s :id)) :test #'equal))

(defun %tutor-lesson (stage id)
  (find id (getf stage :lessons) :key (lambda (l) (getf l :id)) :test #'equal))

(defun %tutor-path ()
  "Every lesson in the tutorial as (STAGE . LESSON), in order — so `next' walks
off the end of a stage into the one after it rather than stopping there."
  (loop for stage in *tutor-stages*
        append (loop for lesson in (getf stage :lessons)
                     collect (cons stage lesson))))

(defun %tutor-exercises (lesson) (getf lesson :exercises))

(defun %tutor-key (stage lesson index)
  "What progress is filed under: three ids, never three positions."
  (list (getf stage :id) (getf lesson :id) index))

(defun %tutor-find (key)
  "(STAGE LESSON EXERCISE) for the exercise KEY names, or NIL for all three
when the data no longer has it — a lesson deleted from `*tutor-stages*' must
leave a stale progress file harmless rather than fatal."
  (let* ((stage (and key (%tutor-stage (first key))))
         (lesson (and stage (%tutor-lesson stage (second key))))
         (exercise (and lesson (nth (third key) (%tutor-exercises lesson)))))
    (values stage lesson exercise)))

(defun %tutor-passedp (stage lesson index)
  (member (%tutor-key stage lesson index) *tutor-passed* :test #'equal))

(defun %tutor-lesson-counts (stage lesson)
  "How many exercises this lesson has, and how many are passed."
  (let ((n (length (%tutor-exercises lesson))))
    (values n (loop for i from 0 below n count (%tutor-passedp stage lesson i)))))

(defun %tutor-counts ()
  "How many exercises the whole tutorial has, and how many are passed."
  (let ((total 0) (done 0))
    (dolist (pair (%tutor-path) (values total done))
      (multiple-value-bind (n k) (%tutor-lesson-counts (car pair) (cdr pair))
        (incf total n)
        (incf done k)))))
;;; ---------------------------------------------------------------------------
;;; Progress
;;;
;;; One file, one form, read with `read' and written with `prin1'. Not a format
;;; of its own: this is Common Lisp, the reader is right there, and a progress
;;; file you can open and edit is a feature rather than an accident.
;;;
;;; `*read-eval*' is bound off around the read. The file is *data*, nothing in
;;; it should ever be evaluated, and `#.' in a file the editor reads on startup
;;; is precisely the hole you do not want to leave open.

(defvar *tutor-place* nil
  "(STAGE-ID . LESSON-ID) — where you were. DEFVAR, so reloading the config in
the middle of a lesson does not send you back to the beginning.")

(defvar *tutor-passed* nil
  "List of (STAGE-ID LESSON-ID INDEX) for every exercise that has passed.")

(defvar *tutor-answers* nil
  "Alist of (STAGE-ID LESSON-ID INDEX) -> the text you last had in that answer
area. Saved as well as the pass marks, so leaving a lesson half-done and coming
back — or restarting the editor — finds your working still there.")

(defun %tutor-progress-text ()
  (with-output-to-string (out)
    (write-string ";;; zemacs-tutor — where you got to.
;;;
;;; Ordinary Lisp data, written by the tutor and read by it on startup. Delete
;;; this file to begin the tutorial again, or edit it: nothing in here is a
;;; secret and nothing in here is evaluated.

" out)
    (with-standard-io-syntax
      (let ((*package* (find-package :zemacs)))
        (prin1 (list :place *tutor-place*
                     :passed *tutor-passed*
                     :answers *tutor-answers*)
               out)))
    (terpri out)))

(defun tutor-save-progress ()
  "Write the pass marks, your answers and your place to disk. Called after
every check and every move; there is nothing to remember to do."
  (handler-case
      (progn
        (ensure-directories-exist *tutor-progress-file*)
        (with-open-file (out *tutor-progress-file*
                             :direction :output
                             :if-exists :supersede
                             :if-does-not-exist :create
                             :external-format :utf-8)
          (write-string (%tutor-progress-text) out))
        t)
    (error (e) (message (format nil "tutor: cannot save progress — ~a" e)) nil)))

(defun tutor-load-progress ()
  "Read the progress file back, if there is one. A file that has been edited
into something unreadable costs you your place and nothing else — the tutorial
still opens, at lesson one."
  (handler-case
      (when (probe-file *tutor-progress-file*)
        (let ((data (with-open-file (in *tutor-progress-file*
                                        :direction :input
                                        :external-format :utf-8)
                      (let ((*read-eval* nil)
                            (*package* (find-package :zemacs)))
                        (read in nil nil)))))
          (when (listp data)
            (setf *tutor-place* (getf data :place)
                  *tutor-passed* (getf data :passed)
                  *tutor-answers* (getf data :answers))
            t)))
    (error (e)
      (message (format nil "tutor: ignoring unreadable progress file — ~a" e))
      nil)))

;;; ---------------------------------------------------------------------------
;;; Stage 1: the child process
;;;
;;; `ext:run-program' with `:wait nil' hands back a stream and a process handle,
;;; and the loop below does three things at once with them: drains the pipe so a
;;; chatty answer cannot fill it and deadlock, asks whether the child has
;;; finished, and gives up after `*tutor-timeout*'. ECL has no timeout of its
;;; own, and this is the whole of adding one — `external-process-wait' with a
;;; NIL second argument polls rather than blocks, and `terminate-process' with
;;; a true second argument is SIGKILL.

(defun %tutor-form-string (form)
  "FORM printed so that reading it back gives FORM again.
`with-standard-io-syntax' guarantees the round trip; `*package*' is then bound
to ZEMACS so that symbols print bare. That matters: the child reads them in
CL-USER, which uses COMMON-LISP, so every standard operator and every symbol
your own answer defines lands on the same symbol at both ends. A check form
naming something in ZEMACS *would* break — which is why Stage 1's checks use
nothing but Common Lisp and what your answer defined, and Stage 2, whose checks
are full of `point' and `overlays-in', is evaluated here instead."
  (with-standard-io-syntax
    (let ((*package* (find-package :zemacs)))
      (prin1-to-string form))))

(defun %tutor-child-program (source check)
  "The program handed to the child: evaluate SOURCE, then decide CHECK over the
value it produced, then print one verdict line.

Two things about its shape are deliberate. SOURCE goes in as a *string* and is
`read' at run time, so unbalanced parentheses are an error the harness catches
rather than a corruption of the harness. And CHECK likewise goes in as a string
and is `read' and `eval'ed at run time — because an exercise that asks you to
write a macro must have that macro *defined* before the form testing it is
compiled, and a check spliced in as literal source would have been compiled
first and would fail on every macro lesson in the file."
  (with-standard-io-syntax
    (let ((*package* (find-package :zemacs)))
      (format nil ";;; Written by zemacs-tutor before each Stage 1 check, and overwritten by
;;; the next one. Run it yourself with `ecl --norc --shell' if a verdict ever
;;; looks wrong; there is nothing in here but your answer and the test.
(setf *print-length* 24 *print-level* 4 *print-circle* t)

(defun tutor-flat (e)
  (substitute #\\Space #\\Newline
              (handler-case (princ-to-string e)
                (serious-condition () \"unprintable condition\"))))

(let ((value nil) (trouble nil))
  (handler-case
      (with-input-from-string (in ~s)
        (loop for form = (read in nil :tutor-eof)
              until (eq form :tutor-eof)
              do (setf value (eval form))))
    (serious-condition (e) (setf trouble (tutor-flat e))))
  (if trouble
      (format t \"~~&~a~~a~~%\" (concatenate 'string \"ERROR \" trouble))
      (handler-case
          (let ((ok (eval (list 'let
                                (list (list 'value (list 'quote value)))
                                (read-from-string ~s)))))
            (format t \"~~&~a~~a~~%\" (if ok \"PASS\" \"FAIL\")))
        (serious-condition (e)
          (format t \"~~&~a~~a~~%\" (concatenate 'string \"FAIL \" (tutor-flat e)))))))
"
              source
              *tutor-verdict-mark*
              (%tutor-form-string check)
              *tutor-verdict-mark*
              *tutor-verdict-mark*))))

(defun %tutor-spawn (program args)
  "Run PROGRAM with ARGS, no stdin, stderr merged into stdout. Answers a status
— :EXITED, :TIMEOUT or :BROKEN — and everything the child said before then."
  (handler-case
      (multiple-value-bind (stream code process)
          (ext:run-program program args
                           :input nil :output :stream :error :output :wait nil)
        (declare (ignore code))
        (unwind-protect
             (let ((text (make-string-output-stream))
                   (deadline (+ (get-internal-real-time)
                                (* *tutor-timeout*
                                   internal-time-units-per-second))))
               (loop
                 ;; Drain first, always. A child that fills the pipe blocks in
                 ;; `write' and would then never reach the exit we are polling
                 ;; for, which is a hang built out of two things that are each
                 ;; individually correct.
                 (loop while (listen stream)
                       do (let ((c (read-char stream nil nil)))
                            (if c (write-char c text) (return))))
                 (unless (eq (ext:external-process-wait process nil) :running)
                   (loop for c = (read-char stream nil nil)
                         while c do (write-char c text))
                   (return (values :exited (get-output-stream-string text))))
                 (when (> (get-internal-real-time) deadline)
                   (ext:terminate-process process t)
                   (return (values :timeout (get-output-stream-string text))))
                 (sleep 0.02)))
          (ignore-errors (close stream))))
    (serious-condition (e) (values :broken (%condition-string e)))))

(defun %tutor-detail (text)
  "TEXT trimmed, or NIL when there is nothing in it. A bare `FAIL' means `that
is not the answer', and `FAIL <condition>' means `the test itself could not
run' — so the difference between an empty string and no string is a different
sentence to the student, and the empty string must not survive this far."
  (let ((trimmed (%tutor-trim text)))
    (when (plusp (length trimmed)) trimmed)))

(defun %tutor-verdict (output)
  "(STATUS . DETAIL) from the last verdict line in OUTPUT, or NIL when there is
none — which means the child died before it could decide."
  (let ((found nil))
    (dolist (line (%tutor-lines output) found)
      (when (%tutor-prefixp *tutor-verdict-mark* line)
        (let ((rest (subseq line (length *tutor-verdict-mark*))))
          (setf found
                (cond ((%tutor-prefixp "PASS" rest) (cons :pass nil))
                      ((%tutor-prefixp "FAIL" rest)
                       (cons :fail (%tutor-detail (subseq rest 4))))
                      ((%tutor-prefixp "ERROR" rest)
                       (cons :error (%tutor-detail (subseq rest 5))))
                      (t found))))))))

(defvar *tutor-no-ecl-warned* nil
  "Whether the `no ecl on PATH' message has been given. Once per session: it is
a real limitation and worth saying, and worth saying only once.")

(defun %tutor-ecl-p ()
  "Whether the system `ecl' can be found. `executable-find' comes from
`ai.lisp', which loads from the same list this file does; without it we assume
yes and let the failed exec below tell us otherwise."
  (if (fboundp 'executable-find)
      (and (executable-find *tutor-ecl*) t)
      t))

(defun %tutor-check-child (source check)
  "Mark SOURCE against CHECK in a child `ecl'. Answers (STATUS . DETAIL) where
STATUS is one of :PASS :FAIL :ERROR :TIMEOUT :BROKEN."
  (handler-case
      (progn
        (ensure-directories-exist *tutor-check-file*)
        (with-open-file (out *tutor-check-file*
                             :direction :output
                             :if-exists :supersede
                             :if-does-not-exist :create
                             :external-format :utf-8)
          (write-string (%tutor-child-program source check) out))
        (multiple-value-bind (status output)
            (%tutor-spawn *tutor-ecl*
                          (list "--norc" "--shell"
                                (namestring *tutor-check-file*)))
          (let ((verdict (%tutor-verdict output)))
            (cond (verdict verdict)
                  ((eq status :timeout)
                   (cons :timeout
                         (format nil "no answer in ~d seconds — an infinite loop?"
                                 *tutor-timeout*)))
                  ((eq status :broken) (cons :broken output))
                  ;; Exited without a verdict. Either the binary is not there
                  ;; (`exec: No such file or directory' on the merged stream) or
                  ;; something ate the harness; both read the same to the
                  ;; caller, which falls back to this image.
                  (t (cons :broken (%tutor-trim output)))))))
    (serious-condition (e) (cons :broken (%condition-string e)))))

;;; ---------------------------------------------------------------------------
;;; Stage 2, and the fallback for Stage 1
;;;
;;; `%eval-source' is `repl.lisp''s, unchanged: `handler-case' on
;;; `serious-condition' rather than on `error', because a stack overflow is not
;;; an `error' in Common Lisp and is exactly what a beginner's recursion
;;; produces. The check is `eval'ed rather than `funcall'ed for the same reason
;;; the child reads it at run time — a macro defined by the answer has to be
;;; there before the test naming it is compiled.

(defun %tutor-check-image (source check)
  "Mark SOURCE against CHECK in *this* image. Answers (STATUS . DETAIL).

ponytail: there is no timeout here and there cannot be one. The Lisp thread
runs one form at a time and nothing preempts it, so a Stage 2 answer containing
`(loop)' costs you the image — the editor keeps drawing and keeps taking your
keystrokes, and files can still be saved, but no further Lisp runs. That is the
ceiling `docs/boundary.org' already records for `C-c', and the fix is the same
one: a child process, which Stage 2 cannot use because its exercises are about
this editor. The prose of Stage 2's first lesson says so out loud."
  (multiple-value-bind (value error) (%eval-source source)
    (if error
        (cons :error error)
        (handler-case
            (if (eval `(let ((value ',value)) ,check))
                (cons :pass nil)
                (cons :fail nil))
          (serious-condition (e) (cons :fail (%condition-string e)))))))

(defun %tutor-check-source (stage source check)
  "Mark SOURCE the way STAGE asks for, degrading gracefully when it cannot.

A stage says `:child' or `:image'. `:child' becomes `:image' when there is no
`ecl' on PATH, with one message saying so — a tutorial that refuses to open
because a binary is missing helps nobody, and in-image checking is right for
every exercise here that does not loop forever."
  (if (and (eq (getf stage :check) :child) (%tutor-ecl-p))
      (let ((verdict (%tutor-check-child source check)))
        (if (eq (car verdict) :broken)
            (progn
              (unless *tutor-no-ecl-warned*
                (setf *tutor-no-ecl-warned* t)
                (message (format nil "tutor: ~a would not run (~a) — marking in this image instead"
                                 *tutor-ecl* (%tutor-one-line-short (cdr verdict)))))
              (%tutor-check-image source check))
            verdict))
      (progn
        (unless (or *tutor-no-ecl-warned* (eq (getf stage :check) :image))
          (setf *tutor-no-ecl-warned* t)
          (message (format nil "tutor: no `~a' on PATH — marking in this image, so a runaway answer will hang it"
                           *tutor-ecl*)))
        (%tutor-check-image source check))))


;;; ---------------------------------------------------------------------------
;;; Reading the document back
;;;
;;; The tutor buffer is org, so it is read the way org is read: one
;;; `buffer-string' and a walk over its lines, with `%org-line-level' saying
;;; what a heading is and `%org-property' reading a drawer line. All three come
;;; from `org-modern.lisp' and none of them is reimplemented here, deliberately
;;; — a second reading of what a heading is would drift from the one that draws
;;; them, and `math.lisp' makes the same choice for the same reason.
;;;
;;; ponytail: every question rescans the buffer. That is a `buffer-string' and a
;;; pass over its lines — a millisecond or so on a document this size, and
;;; correct by construction, since nothing can go stale if nothing is
;;; remembered. Nothing here runs per keystroke, which is the only reason it is
;;; affordable; the upgrade path, if a lesson ever wants a cursor hook, is the
;;; change *delta* `docs/boundary.org' lists as missing.

(defun %tutor-buffer-p (&optional (lines (%org-lines)))
  "T when the live buffer declares itself the tutor with `#+ZEMACS_TUTOR'.

Searched in the *preamble* only — the lines above the first heading — which is
where org puts keywords, and what stops a lesson quoting the declaration from
turning the buffer it is quoted in into a second tutor."
  (let ((want (concatenate 'string "#+" *tutor-keyword* ":")))
    (dolist (l lines nil)
      (let ((text (first l)))
        (when (%org-line-level text) (return nil))
        (let ((s (%tutor-trim text)))
          (when (and (>= (length s) (length want))
                     (string-equal want s :end2 (length want)))
            (return t)))))))

(defun %tutor-tagged (property &optional (lines (%org-lines)))
  "Every heading carrying PROPERTY, in document order, as (WORDS BEGIN END TEXT)
— WORDS the property's value split on spaces, BEGIN and END the heading *line*,
and TEXT that line as it stands.

The heading rather than the property line, for `%org-id-heading''s reason: a
drawer belongs to the headline above it, and landing on `:ZEMACS_TUTOR_EXERCISE:'
would put point inside a drawer, which is not where anyone meant to go."
  (let ((out nil) (head nil))
    (dolist (l lines (nreverse out))
      (destructuring-bind (text begin end) l
        (cond ((%org-line-level text) (setf head (list begin end text)))
              (head
               (let ((v (%org-property text property)))
                 (when v
                   (push (cons (%tutor-words v) head) out)
                   (setf head nil)))))))))

(defun %tutor-exercise-key (words)
  "(STAGE-ID LESSON-ID INDEX) from an exercise property's three words, or NIL.
A malformed drawer costs that one exercise rather than the scan."
  (when (= 3 (length words))
    (let ((n (parse-integer (third words) :junk-allowed t)))
      (when n (list (first words) (second words) n)))))

(defun %tutor-lesson-at-point ()
  "The (STAGE-ID LESSON-ID) of the lesson point is in, or NIL — which is the
honest answer in the Contents and in the preamble above it."
  (let ((at (point)) (found nil))
    (dolist (l (%tutor-tagged *tutor-lesson-property*) found)
      (when (<= (second l) at) (setf found (first l))))))

(defun %tutor-contents-span (&optional (lines (%org-lines)))
  "(BEGIN . END) of the whole Contents section, or NIL.

BEGIN is its heading's first star and END the first star of the *next*
top-level heading, so replacing the range rewrites the section and leaves the
lessons after it untouched."
  (let ((begin nil) (end nil) (limit 0))
    (dolist (l lines)
      (destructuring-bind (text b e) l
        (setf limit e)
        (let ((level (%org-line-level text)))
          (cond ((and begin (null end) (eql level 1)) (setf end b))
                ((and (null begin) (eql level 1)
                      (string-equal *tutor-contents-heading*
                                    (%tutor-trim (subseq text level))))
                 (setf begin b))))))
    (when begin (cons begin (or end limit)))))

;;; ---------------------------------------------------------------------------
;;; Writing the document
;;;
;;; One function per part, all of them pure: they take the data and the progress
;;; and answer a string. Nothing here touches the editor, which is what makes
;;; the whole rendering testable by printing it.

(defun %tutor-org-id (stage lesson)
  "The `:ID:' a lesson's heading carries — org's own property, so an
`[[id:...]]' link in the Contents is followed by `org-open-at-point' with
nothing in this file involved."
  (format nil "tutor-~a-~a" (getf stage :id) (getf lesson :id)))

(defparameter *tutor-legend*
  "Instruction on this side, your answers on the other. Press =RET= on a lesson
below to open it: the pane on the right fills with the exercise you are on, and
=SPC m c= marks what you have written there.

- =RET= — open the lesson under point
- =SPC m n= / =SPC m p= — the next or previous exercise
- =SPC m c= — check the answer beside this lesson
- =SPC m h= — a hint for it
- =SPC m u= — put the starting template back
- =SPC m g= — back to this page
- =SPC m w= — the scratch buffer Stage 2's exercises act on
- =M-o= — move between the two panes
- =z a= — fold the lesson or the stage under point
- =q= — leave

"
  "What the Contents says above the list. A `defparameter' rather than a string
literal buried in the renderer, because it is the one part of the document a
config might reasonably want to say differently.")

(defun %tutor-contents-text ()
  "The whole `* Contents' section, with the progress marks worked out fresh.

Ordinary org: a checkbox per lesson, which `org-modern' draws as a box, and an
`[[id:...]]' link, which it draws as its description alone. So the line you
read is `☑ 3. Lists are a story told by cons cells — 3/3' and the line in the
buffer is something a text editor can search."
  (multiple-value-bind (total done) (%tutor-counts)
    (with-output-to-string (out)
      (format out "* ~a~%~%" *tutor-contents-heading*)
      (format out "*~d of ~d exercises done.*~%~%" done total)
      (write-string *tutor-legend* out)
      (let ((n 0))
        (dolist (stage *tutor-stages*)
          (format out "*~a*~%~%" (getf stage :title))
          (dolist (lesson (getf stage :lessons))
            (incf n)
            (multiple-value-bind (k k-done) (%tutor-lesson-counts stage lesson)
              (format out "- [~a] [[id:~a][~d. ~a]]~@[ — ~a~]~%"
                      (if (and (plusp k) (= k k-done)) "X" " ")
                      (%tutor-org-id stage lesson)
                      n (getf lesson :title)
                      (when (plusp k) (format nil "~d/~d" k-done k)))))
          (terpri out))))))

(defun %tutor-lesson-org (stage lesson number)
  "One lesson as org: a heading carrying both ids, the prose as written, and a
sub-heading per exercise carrying the three ids that name it.

The prose goes out with `write-string' and not with `format': a lesson is
allowed to contain a tilde, and a document that broke because somebody wrote
about `~a' would be a very silly bug."
  (with-output-to-string (out)
    (format out "~%** ~d. ~a~%" number (getf lesson :title))
    (format out ":PROPERTIES:~%:~a: ~a~%:~a: ~a ~a~%:END:~%~%"
            *tutor-id-property* (%tutor-org-id stage lesson)
            *tutor-lesson-property* (getf stage :id) (getf lesson :id))
    (write-string (getf lesson :prose) out)
    (terpri out)
    (let ((index -1))
      (dolist (exercise (%tutor-exercises lesson))
        (incf index)
        (format out "~%*** Exercise ~d~@[ ~a~]~%" (1+ index)
                (when (%tutor-passedp stage lesson index) *tutor-done-mark*))
        (format out ":PROPERTIES:~%:~a: ~a ~a ~d~%:END:~%~%"
                *tutor-exercise-property*
                (getf stage :id) (getf lesson :id) index)
        (write-string (getf exercise :prompt) out)
        (terpri out)))
    (when (getf lesson :closing)
      (terpri out)
      (write-string (getf lesson :closing) out)
      (terpri out))))

(defun %tutor-org-text ()
  "The whole tutorial as one org document. Lessons are numbered across the
stages rather than within them, so the number in the Contents and the number on
the heading are the same number."
  (with-output-to-string (out)
    (format out "#+TITLE: zemacs tutor~%")
    (format out "#+~a: 1~%~%" *tutor-keyword*)
    (write-string (%tutor-contents-text) out)
    (let ((n 0))
      (dolist (stage *tutor-stages*)
        (format out "~%* ~a~%" (getf stage :title))
        (dolist (lesson (getf stage :lessons))
          (incf n)
          (write-string (%tutor-lesson-org stage lesson n) out))))))

(defun %tutor-strip-mark (title)
  "TITLE without a trailing done mark."
  (let ((n (length *tutor-done-mark*)))
    (%tutor-trim
     (if (and (>= (length title) n)
              (string= *tutor-done-mark* title :start2 (- (length title) n)))
         (subseq title 0 (- (length title) n))
         title))))

(defun %tutor-mark (key donep)
  "Put KEY's done mark on its heading, or take it off, in one `replace-region'.

The line is *rebuilt* rather than having a character appended, because
rebuilding is idempotent and appending is not: checking a passed exercise again
would otherwise grow a second tick. One call and not two, for the reason
`init.lisp' gives at the top of the file — Lisp here is not atomic with respect
to your typing, and a heading half-rewritten is a document rather than a
status."
  (let ((hit (find key (%tutor-tagged *tutor-exercise-property*)
                   :key (lambda (e) (%tutor-exercise-key (first e)))
                   :test #'equal)))
    (when hit
      (destructuring-bind (words begin end text) hit
        (declare (ignore words))
        (let* ((level (%org-line-level text))
               (title (%tutor-strip-mark (%tutor-trim (subseq text level))))
               (want (format nil "~a ~a~@[ ~a~]"
                             (make-string level :initial-element #\*)
                             title (and donep *tutor-done-mark*))))
          (unless (string= want text) (replace-region begin end want))
          t)))))

;;; ---------------------------------------------------------------------------
;;; The two panes
;;;
;;; `%repl-show' is the worked example and this is it one size up: read
;;; `window-id' *before* the split, because `split-window-right' makes the new
;;; pane current, and hand focus back at the end.
;;;
;;; Every window here is *found* rather than remembered, with one exception. A
;;; window showing a named buffer is one `window-list' away and cannot go stale;
;;; the answer pane is remembered as well, because `SPC m w' may have put the
;;; scratch buffer in it and `window-list' can then no longer tell it from any
;;; other pane.

(defvar *tutor-answer-window* nil
  "The right-hand pane's window id, so that it can be found again even while it
is showing something other than the answer buffer.")

(defun %tutor-window (name)
  "The id of a window in the focused frame showing buffer NAME, or NIL."
  (first (find name (window-list) :key #'second :test #'string=)))

(defun %tutor-answer-window ()
  "The right-hand pane, if it is still there."
  (let ((id *tutor-answer-window*))
    (when (and id (find id (window-list) :key #'first)) id)))

(defun %tutor-instruction-buffer ()
  "Build the instruction pane's buffer and fill it with the whole tutorial.

`create-buffer' first: it is idempotent by name and applied on the spot, so
this both makes the buffer the first time and guarantees the `replace-region'
lands in it rather than in whatever you were looking at.

`set-major-mode' *after* the text, so that the org-mode body — which is what
runs `org-modern-refresh' and draws all of this — arrives to a document rather
than to an empty buffer. It is delivered on the next turn of the application
loop, as every mode hook is; the minor mode is switched on here as well as from
`*org-mode-functions*' so that the keymap is live before your next keystroke
rather than a frame later."
  (create-buffer *tutor-buffer* "org")
  (replace-region 0 (point-max) (%tutor-org-text))
  (goto-char 0)
  (set-major-mode "org-mode")
  (unless (minor-mode-p 'tutor-lesson) (set-minor-mode "tutor-lesson" t))
  (set-evil-state "normal")
  *tutor-buffer*)

(defun %tutor-answer-buffer ()
  "Build the answer pane's buffer: an ordinary `lisp-mode' buffer, so it has the
indenter, parinfer and `C-c C-e' to evaluate a form while you are working on
it. `C-c C-c' is the one key the tutor takes from it — minor modes are
consulted before the major mode, so here it checks rather than evaluating."
  (create-buffer *tutor-lisp-buffer* "lisp")
  (set-major-mode "lisp-mode")
  (unless (minor-mode-p 'tutor-answer) (set-minor-mode "tutor-answer" t))
  *tutor-lisp-buffer*)

(defun %tutor-layout ()
  "Two panes, instruction left and answers right. Answers the left window's id.

A second `M-x zemacs-tutor' reuses the panes rather than splitting the frame
into narrower and narrower columns, which is the whole reason the answer
window is remembered."
  (%tutor-instruction-buffer)
  (let* ((left (window-id))
         (right (let ((w (%tutor-answer-window))) (and w (/= w left) w))))
    (if right (select-window right) (split-window-right))
    (setf *tutor-answer-window* (window-id))
    (%tutor-answer-buffer)
    (select-window left)
    left))

(defun %tutor-goto-instruction ()
  "Put focus in the instruction pane, building the tutor if it is not up."
  (if (member *tutor-buffer* (buffer-names) :test #'string=)
      (let ((w (%tutor-window *tutor-buffer*)))
        (if w (select-window w) (switch-to-buffer *tutor-buffer*)))
      (%tutor-layout)))

(defun %tutor-write-answer (text)
  "Put TEXT in the answer pane, leaving focus where it was.

Selecting the window rather than reaching for `with-current-buffer' from
wherever we happen to be: a buffer switch changes what the *current* window
shows, and doing that from the instruction pane would flash the answer buffer
over the lesson you are reading. The fallback is for a frame with no second
pane, where a brief switch is the only way in and is over in one call."
  (let ((here (window-id))
        (there (%tutor-answer-window)))
    (cond ((and there (/= there here))
           (select-window there)
           (switch-to-buffer *tutor-lisp-buffer*)
           (replace-region 0 (point-max) text)
           (goto-char 0)
           (select-window here))
          (t (with-current-buffer *tutor-lisp-buffer*
               (replace-region 0 (point-max) text)
               (goto-char 0))))))

(defun %tutor-read-answer ()
  "Whatever is in the answer pane, or NIL when there is no such buffer.
A read and nothing else, which is what `with-current-buffer' is for."
  (with-current-buffer *tutor-lisp-buffer* (buffer-string)))

(defun %tutor-mark-in-place (key donep)
  "Put KEY's done mark on in the instruction pane, leaving focus where it was."
  (let ((here (window-id))
        (there (%tutor-window *tutor-buffer*)))
    (cond ((and there (/= there here))
           (select-window there)
           (%tutor-mark key donep)
           (select-window here))
          (t (with-current-buffer *tutor-buffer* (%tutor-mark key donep))))))

;;; ---------------------------------------------------------------------------
;;; The scratch buffer Stage 2 writes in
;;;
;;; A third buffer and not a third pane. Stage 2's exercises insert text,
;;; replace the buffer whole and hang overlays on it, and doing that to the pane
;;; holding your answer would destroy the thing being marked. `SPC m w' shows
;;; it; anything that selects an exercise puts your answer back.

(defun %tutor-playground-text ()
  ";;; *tutor-scratch* — Stage 2 writes here.
;;;
;;; Every Stage 2 exercise is evaluated with *this* buffer live, so `(insert
;;; \"x\")' inserts here, `(point)' answers about here, and an overlay you make
;;; lands here. Scribble in it as much as you like: the checks only ever look at
;;; what your own answer did.
;;;
;;; `SPC m w' shows it and `SPC m W' puts this text back.

(defun hello (name)
  (format nil \"hello, ~a\" name))
")

(defun %tutor-ensure-playground ()
  "Make the scratch buffer if it is not there, without spending a pane on it.

`create-buffer' shows what it makes in the current window, so the buffer that
was there is put back — the pane the user is looking at is not ours to take."
  (unless (member *tutor-playground* (buffer-names) :test #'string=)
    (let ((was (buffer-name)))
      (create-buffer *tutor-playground* "lisp")
      (replace-region 0 (point-max) (%tutor-playground-text))
      (goto-char 0)
      (when was (switch-to-buffer was))))
  *tutor-playground*)

(defun tutor-show-scratch ()
  "Show the buffer Stage 2's exercises act on, in the answer pane."
  (%tutor-ensure-playground)
  (let ((here (window-id))
        (there (%tutor-answer-window)))
    (when (and there (/= there here)) (select-window there))
    (switch-to-buffer *tutor-playground*)
    (select-window here))
  (message "tutor: the scratch buffer — SPC m n or SPC m p brings your answer back"))

(defun tutor-reset-playground ()
  "Put the Stage 2 scratch buffer back the way it started."
  (%tutor-ensure-playground)
  (with-current-buffer *tutor-playground*
    (replace-region 0 (point-max) (%tutor-playground-text))
    (goto-char 0))
  (message "tutor: scratch buffer reset"))

;;; ---------------------------------------------------------------------------
;;; Which exercise is selected
;;;
;;; One variable, and it is the answer to "what does `SPC m c' check". Derived
;;; from point in the instruction pane would be the other design and is worse:
;;; the check is also reachable from the answer pane, where point means nothing
;;; about the lesson, and scrolling past an exercise should not silently change
;;; what a keypress does.

(defvar *tutor-current* nil
  "(STAGE-ID LESSON-ID INDEX) of the exercise the answer pane is holding, or
NIL at the contents. DEFVAR: a config reload in the middle of a lesson should
not lose your place in it.")

(defun %tutor-answer-for (key)
  "What the answer pane should hold for KEY: what you last had there, else the
exercise's template. Templates are written to *run* and be wrong — the first
check of an exercise is meant to come back `not yet', because seeing the loop
work once is worth more than reading a paragraph about it."
  (or (cdr (assoc key *tutor-answers* :test #'equal))
      (multiple-value-bind (stage lesson exercise) (%tutor-find key)
        (declare (ignore stage lesson))
        (or (getf exercise :template) ""))))

(defun %tutor-harvest ()
  "Copy the answer pane into `*tutor-answers*' under the selected exercise, so
that moving to another one never loses your working."
  (when *tutor-current*
    (let ((text (%tutor-read-answer)))
      (when text
        (let ((old (assoc *tutor-current* *tutor-answers* :test #'equal)))
          (if old
              (setf (cdr old) text)
              (push (cons *tutor-current* text) *tutor-answers*)))))))

(defun %tutor-describe (key)
  "One line about KEY for the status line."
  (multiple-value-bind (stage lesson) (%tutor-find key)
    (if (null lesson)
        "tutor: that exercise is no longer in the lessons"
        (multiple-value-bind (n done) (%tutor-lesson-counts stage lesson)
          (format nil "exercise ~d of ~d · ~d done · ~a"
                  (1+ (third key)) n done (getf lesson :title))))))

(defun %tutor-select (key)
  "Make KEY the exercise the answer pane holds."
  (%tutor-harvest)
  (setf *tutor-current* key)
  (%tutor-write-answer (%tutor-answer-for key))
  (tutor-save-progress)
  key)

;;; ---------------------------------------------------------------------------
;;; Moving about
;;;
;;; Every one of these leaves focus in the pane it was called from. A command
;;; that stole the cursor would mean pressing `SPC m n' from the answer pane and
;;; then having to come back to type, which is the gesture this whole layout
;;; exists to make short.

(defun %tutor-step (delta)
  "Move to the exercise DELTA away from the selected one and select it.

Counted from the *selection* and not from point, and that is the difference
between `next' meaning what you expect and `next' meaning nothing: `RET' on a
lesson leaves point at its heading, so that you can read the lesson, while its
first exercise is already on the right — and a step measured from point would
find that same first exercise again and go nowhere.

Point is the fallback, for the one state where there is no selection to count
from: the contents, where the first exercise below where you are reading is the
only sensible answer."
  (let ((here (window-id)))
    (%tutor-goto-instruction)
    (let* ((all (%tutor-tagged *tutor-exercise-property*))
           (keys (mapcar (lambda (e) (%tutor-exercise-key (first e))) all))
           (at (and *tutor-current* (position *tutor-current* keys :test #'equal)))
           (n (cond (at (+ at delta))
                    ((plusp delta)
                     (position-if (lambda (e) (> (second e) (point))) all))
                    (t (position-if (lambda (e) (< (second e) (point))) all
                                    :from-end t))))
           (key (and n (>= n 0) (< n (length keys)) (nth n keys))))
      (cond (key (goto-char (second (nth n all)))
                 (%tutor-select key)
                 (message (%tutor-describe key)))
            (t (message (if (plusp delta)
                            "tutor: no exercise after this one — SPC m g for the contents"
                            "tutor: no exercise before this one")))))
    (select-window here)))

(defun tutor-next-exercise ()
  "The next exercise: point moves in the lesson, the answer pane fills."
  (%tutor-step 1))

(defun tutor-previous-exercise ()
  "The previous exercise."
  (%tutor-step -1))

(defun %tutor-select-here ()
  "Select the first exercise of the lesson point is in, preferring the first one
still to do — which is where you left off."
  (let ((words (%tutor-lesson-at-point)))
    (when words
      (multiple-value-bind (stage lesson)
          (%tutor-find (list (first words) (second words) 0))
        (when lesson
          (let ((n (length (%tutor-exercises lesson))))
            (when (plusp n)
              (let* ((index (or (loop for i from 0 below n
                                      unless (%tutor-passedp stage lesson i) return i)
                                0))
                     (key (%tutor-key stage lesson index)))
                (%tutor-select key)
                (message (%tutor-describe key))
                key))))))))

(defun tutor-open-at-point ()
  "Follow the link under point, and load the lesson you land in.

`RET' in the tutor. The following is `org-open-at-point' unmodified — the
Contents is written with ordinary `[[id:...]]' links, so this file contributes
no link machinery at all. What it adds is the second half of the gesture: the
answer pane fills with the first exercise of wherever you landed."
  (org-open-at-point)
  (%tutor-select-here))

(defun tutor-contents ()
  "Back to the table of contents, with the progress marks brought up to date.

One `replace-region' over the whole section rather than a line per lesson: the
counts move whenever anything passes, and rewriting the page you are about to
look at is both simpler and — being one call — not interruptible halfway."
  (%tutor-harvest)
  (%tutor-goto-instruction)
  (let ((span (%tutor-contents-span)))
    (cond (span (replace-region (car span) (cdr span) (%tutor-contents-text))
                (goto-char (car span)))
          (t (message "tutor: no Contents section — M-x zemacs-tutor builds one")))))

(defun %tutor-label (pair at)
  "One row of the jump list: number, title, and how much of it is done."
  (destructuring-bind (stage . lesson) pair
    (multiple-value-bind (n done) (%tutor-lesson-counts stage lesson)
      (format nil "~2d. ~a — ~a  [~d/~d]"
              at (getf stage :title) (getf lesson :title) done n))))

(defun tutor-goto-lesson ()
  "Jump to any lesson, by name.

`completing-read' answers on a later turn of the Lisp queue — it is a
continuation and not a blocking read, for the reason `docs/threading.org'
gives — so the rest of this command lives inside the callback."
  (%tutor-harvest)
  (let* ((path (%tutor-path))
         (rows (loop for pair in path
                     for at from 1
                     collect (%tutor-label pair at))))
    (completing-read "lesson: " rows
                     (lambda (answer)
                       (let ((i (position answer rows :test #'string=)))
                         (cond ((null i) (when answer (message "tutor: no such lesson")))
                               (t (let ((pair (nth i path)))
                                    (%tutor-goto-instruction)
                                    (let ((at (%org-id-heading
                                               (%tutor-org-id (car pair) (cdr pair)))))
                                      (when at (goto-char at))
                                      (%tutor-select-here))))))))))

;;; ---------------------------------------------------------------------------
;;; Checking

(defun %tutor-run (stage source check)
  "Mark SOURCE against CHECK the way STAGE asks for.

Stage 2 is evaluated with the *scratch* buffer live, and so is its check: an
exercise about `insert' has to insert somewhere, and the question `did it work'
has to be asked in the same place."
  (if (eq (getf stage :check) :image)
      (progn (%tutor-ensure-playground)
             (with-current-buffer *tutor-playground*
               (%tutor-check-source stage source check)))
      (%tutor-check-source stage source check)))

(defun %tutor-report (key verdict)
  "Record VERDICT for KEY, mark its heading, and say what happened."
  (when (eq (car verdict) :pass)
    (pushnew key *tutor-passed* :test #'equal))
  (tutor-save-progress)
  (%tutor-mark-in-place key (eq (car verdict) :pass))
  (let ((n (1+ (third key))))
    (message
     (case (car verdict)
       (:pass (multiple-value-bind (total done) (%tutor-counts)
                (format nil "tutor: exercise ~d passed — ~d of ~d" n done total)))
       (:fail (if (cdr verdict)
                  (format nil "tutor: exercise ~d — your code ran, but the test could not: ~a"
                          n (%tutor-one-line-short (cdr verdict)))
                  (format nil "tutor: exercise ~d — that is not the answer yet. SPC m h for a hint."
                          n)))
       (:error (format nil "tutor: exercise ~d signalled — ~a"
                       n (%tutor-one-line-short (cdr verdict))))
       (:timeout (format nil "tutor: exercise ~d — ~a"
                         n (%tutor-one-line-short (cdr verdict))))
       (t (format nil "tutor: exercise ~d — the marker itself broke: ~a"
                  n (%tutor-one-line-short (cdr verdict))))))))

(defun tutor-check ()
  "Check what is in the answer pane against the selected exercise.

Reachable from either pane, and it always means the same thing: the exercise
`SPC m n' or `RET' last selected, marked against the text on the right."
  (cond
    ((null *tutor-current*)
     (message "tutor: no exercise selected — RET on a lesson, or SPC m n"))
    (t
     (multiple-value-bind (stage lesson exercise) (%tutor-find *tutor-current*)
       (declare (ignore lesson))
       (cond
         ((null exercise)
          (message "tutor: that exercise is no longer in the lessons"))
         (t
          (%tutor-harvest)
          (let ((source (%tutor-answer-for *tutor-current*)))
            (if (%tutor-blankp source)
                (message "tutor: nothing in the answer pane yet — write something first")
                (%tutor-report *tutor-current*
                               (%tutor-run stage source (getf exercise :check)))))))))))

(defun tutor-hint ()
  "Show the hint for the selected exercise."
  (multiple-value-bind (stage lesson exercise) (%tutor-find *tutor-current*)
    (declare (ignore stage lesson))
    (cond ((null exercise) (message "tutor: no exercise selected"))
          ((getf exercise :hint)
           (message (format nil "hint: ~a" (%one-line (getf exercise :hint)))))
          (t (message "tutor: no hint for this one — the lesson above has it")))))

(defun tutor-reset-answer ()
  "Put the starting template back in the answer pane."
  (cond ((null *tutor-current*) (message "tutor: no exercise selected"))
        (t (setf *tutor-answers*
                 (remove *tutor-current* *tutor-answers* :key #'car :test #'equal))
           (%tutor-write-answer (%tutor-answer-for *tutor-current*))
           (tutor-save-progress)
           (message (format nil "tutor: exercise ~d reset" (1+ (third *tutor-current*)))))))

(defun tutor-progress ()
  "Say how far through the tutorial you are."
  (multiple-value-bind (total done) (%tutor-counts)
    (message (format nil "zemacs tutor — ~d of ~d exercise~:p done" done total))))

(defun tutor-quit ()
  "Save and go back to the dashboard. Both buffers stay; `M-x zemacs-tutor'
brings them back, at the contents, with everything you typed still in them."
  (%tutor-harvest)
  (tutor-save-progress)
  (show-dashboard))

(defun tutor-reset-progress ()
  "Forget every pass mark and every answer, and rebuild the tutorial."
  (setf *tutor-passed* nil *tutor-answers* nil *tutor-current* nil)
  (tutor-save-progress)
  (%tutor-layout)
  (tutor-contents)
  (message "tutor: progress cleared"))

;;; ---------------------------------------------------------------------------
;;; The way in

(defvar *tutor-loaded* nil
  "Whether the progress file has been read this session. DEFVAR: reloading the
config must not re-read the file over answers you have typed since.")

(defun zemacs-tutor ()
  "Open the tutorial: the lesson on the left, your answers on the right.

*Always at the table of contents*, and never at the lesson you were last in.
Where you got to is a question a page showing all of it answers better than
being dropped somewhere and left to work it out — and the contents is where the
progress marks are, so opening there is also the only place they are all
visible at once."
  (unless *tutor-loaded*
    (setf *tutor-loaded* t)
    (tutor-load-progress))
  (%tutor-layout)
  (tutor-contents)
  (multiple-value-bind (total done) (%tutor-counts)
    (message (format nil "zemacs tutor — ~d of ~d exercises done. RET on a lesson to begin."
                     done total))))

(defun tutor ()
  "Open the interactive tutorial. The same command as `zemacs-tutor', under the
name you would guess — one of them is what you would type and the other is what
the feature is called, and neither is worth losing to make the M-x list one row
shorter."
  (zemacs-tutor))

;;; ---------------------------------------------------------------------------
;;; The modes
;;;
;;; Two *minor* modes and no major one, and that is the whole presentation in
;;; one decision. The instruction pane has to keep `org-mode' — everything that
;;; makes it look right hangs off it — and the answer pane has to keep
;;; `lisp-mode' for the indenter and the REPL keys. A `tutor-mode' deriving from
;;; either would mean every feature added to that mode afterwards had to
;;; remember to derive too, and `org-modern.lisp' re-declares `org-mode'
;;; deliberately, so a second declaration here would silently replace it.
;;;
;;; What a minor mode buys is a keymap consulted *before* the major mode's. So
;;; `SPC m n' means "next exercise" in the tutor and is free in every other org
;;; buffer, and `C-c C-c' checks here while still evaluating in every other Lisp
;;; buffer.

(define-minor-mode tutor-lesson
  "The tutor's instruction pane: `RET' opens a lesson, `SPC m n' and `SPC m p'
step between exercises, `SPC m c' checks the answer beside them and `SPC m g'
goes back to the contents.")

(define-minor-mode tutor-answer
  "The tutor's answer pane: the same motions as the lesson side, so that
checking or stepping does not mean moving to the other window first.")

(defun tutor-lesson-maybe ()
  "Turn `tutor-lesson' on in a buffer that declares itself the tutor.

On `*org-mode-functions*', so the mode survives re-entering org-mode — which
`%tutor-instruction-buffer' causes on every render. Guarded by `minor-mode-p'
because the org-mode body runs on every entry and the mode command is a toggle:
without the guard, re-rendering would switch the motions off."
  (when (and (%tutor-buffer-p) (not (minor-mode-p 'tutor-lesson)))
    (tutor-lesson)))

;;; DEFVAR before the PUSHNEW, exactly as `org-modern.lisp' and `math.lisp' do:
;;; a build whose `*runtime-dir*' never found that file must get an empty list
;;; here rather than an unbound variable.
(defvar *org-mode-functions* nil)
(pushnew 'tutor-lesson-maybe *org-mode-functions*)

;;; ---------------------------------------------------------------------------
;;; Keys
;;;
;;; The same set in both panes, because the whole point of the layout is that
;;; you are working in two windows and should not have to remember which one you
;;; are in. `RET' and `q' are the exceptions and are the instruction pane's
;;; alone: `q' is a letter you type on the answer side, and `RET' is a newline.
;;;
;;; A mode keymap is consulted from `normal_key' and nowhere else, so none of
;;; this claims a key in Insert state — pressing `q' while typing types a `q'.
;;; That is also why the `C-c C-*' family needs Esc first, exactly as `C-c C-e'
;;; does in every other Lisp buffer: `insert_key' looks up single keys and never
;;; waits for a second, because waiting would swallow text you meant to type.

(dolist (map '("tutor-lesson" "tutor-answer"))
  (define-key map "SPC m c" "tutor-check")
  (define-key map "SPC m h" "tutor-hint")
  (define-key map "SPC m u" "tutor-reset-answer")
  (define-key map "SPC m n" "tutor-next-exercise")
  (define-key map "SPC m p" "tutor-previous-exercise")
  (define-key map "SPC m g" "tutor-contents")
  (define-key map "SPC m j" "tutor-goto-lesson")
  (define-key map "SPC m s" "tutor-progress")
  (define-key map "SPC m w" "tutor-show-scratch")
  (define-key map "SPC m W" "tutor-reset-playground")
  ;; The Emacs spellings, for the fingers that already have them.
  (define-key map "C-c C-c" "tutor-check")
  (define-key map "C-c C-n" "tutor-next-exercise")
  (define-key map "C-c C-p" "tutor-previous-exercise")
  (define-key map "C-c C-h" "tutor-hint")
  (define-key map "C-c C-u" "tutor-reset-answer"))

;;; `RET' follows a link and then loads what it landed on. `SPC m o' is the
;;; leader spelling org itself gives the same gesture, pointed here so that both
;;; halves happen either way.
(define-key "tutor-lesson" "<ret>" "tutor-open-at-point")
(define-key "tutor-lesson" "SPC m o" "tutor-open-at-point")
(define-key "tutor-lesson" "q" "tutor-quit")

;;; The way in, from anywhere. Spelled with `define-key' rather than with the
;;; config's `define-leader' for the reason `library.lisp' gives: a file under
;;; runtime/modes/ has to load whether or not an init file has defined that yet.
;;; `SPC h t' is beside the other help bindings; Emacs puts its own tutorial on
;;; `C-h t'.
(dolist (state '("normal" "visual" "visual-line" "visual-block"))
  (define-key state "SPC h t" "zemacs-tutor"))

;;; The progress file's two doors are nouns rather than verbs — `M-x' is a list
;;; of things to run, and running either of these by hand does nothing you can
;;; see. `*hidden-commands*' is `init.lisp''s existing answer to exactly this.
(when (boundp '*hidden-commands*)
  (dolist (name '("tutor-save-progress" "tutor-load-progress"))
    (pushnew name *hidden-commands* :test #'string=)))
;;; ===========================================================================
;;; The lessons
;;; ===========================================================================
;;;
;;; From here down there is no code, only data. Nothing above dispatches on a
;;; lesson's id, counts the lessons, or knows what any of them is about, so a
;;; lesson is added by adding a list entry and deleted by deleting one.
;;;
;;; A stage:
;;;
;;;   (:id     "cl"                  filed under, in the progress file
;;;    :title  "Stage 1 — Common Lisp"
;;;    :check  :child                or :image; see the header of this file
;;;    :lessons (...))
;;;
;;; A lesson:
;;;
;;;   (:id "quote" :title "..." :prose "..." :exercises (...))
;;;
;;; ...and optionally `:closing', more prose printed under the last exercise.
;;;
;;; An exercise:
;;;
;;;   (:prompt   "what to do"
;;;    :template "code to start from — runnable, and wrong"
;;;    :check    (equal value '(1 2 3))
;;;    :hint     "one sentence, shown on C-c C-h")
;;;
;;; `:check' is a form, not a function, and it is evaluated with `value' bound
;;; to whatever the answer's last form produced. It may call anything the answer
;;; defined, which is how an exercise about `defun' is marked: not by looking at
;;; the text, but by calling the function three times with different arguments.
;;;
;;; Two rules about the check forms in Stage 1, both consequences of their being
;;; evaluated in a child image: they may use *only* Common Lisp and names the
;;; student's own answer defines, and they must terminate. Stage 2's checks run
;;; here and may use anything at all.
;;;
;;; Templates are deliberately wrong. A first `C-c C-c' that comes back `not
;;; yet' has taught the loop; a first one that comes back `passed' has taught
;;; nothing and stolen the exercise. Where there is no plausible wrong start —
;;; the two `read-string' exercises, whose obvious wrong start *is* the answer
;;; with one argument missing — the template is a comment saying what to write,
;;; which evaluates to nothing and fails just as usefully.
;;;
;;; Both halves of that are worth checking when a lesson is added, and the way
;;; to check them is the way `crates/lisp/tests/tutor.rs' does: run the template
;;; against the check and make sure it does *not* pass.

(defparameter *tutor-stage-1*
  '(:id "cl"
    :title "Stage 1 — Common Lisp"
    :check :child
    :lessons

    (;; -----------------------------------------------------------------------
     (:id "reader"
      :title "The reader, and why this language has almost no syntax"
      :prose
"Most languages have a grammar. =if (x > 0) { f(x); }= is parsed by rules
that know what =if= is, what a block is, and where a semicolon may go, and
the result is a tree the compiler understands and you do not get to touch.

Common Lisp has a *reader* instead. The reader knows about four things:
parentheses, whitespace, a handful of characters like the quote mark, and
everything else. It does not know what =if= means. It reads

#+begin_src lisp
(+ 1 2)
#+end_src

and hands back a list of three objects — the symbol +, the integer 1, the
integer 2 — and stops. Deciding that the first element names an operation
is somebody else's job, and that somebody is the evaluator, which runs
afterwards and separately.

That separation is the whole design, and everything the rest of this stage
teaches falls out of it:

  - Your program, after reading, is made of ordinary data: lists, symbols,
    numbers, strings. Not a private tree; the same conses =cons= makes.
  - So a program can build a program, and =defmacro= (lesson 12) is not a
    template language bolted on the side but the obvious consequence.
  - So =print= and =read= are inverses. The transcript in this editor's REPL
    is a legal Lisp file *because* of this, and evaluating it replays the
    session.

The word for a language whose programs are made of its own data structures
is homoiconic. It is a long word for a short idea: there is no separate
program representation.

The reader is a function. You can call it:

#+begin_src lisp
(read-from-string \"(+ 1 2)\")
#+end_src

That answers the list, without evaluating anything. Try it in the first
exercise and look at what you get.

One more thing the reader does, and it surprises everybody once: it upcases.
=foo=, =Foo= and =FOO= all read as the same symbol, whose name is the string
\"FOO\". That is why =symbol-name= answers in capitals throughout this
tutorial, and why the editor's own code writes
=(find-symbol \"TUTOR-CHECK\" :zemacs)= with the name in capitals when it goes
looking for one."
      :exercises
      ((:prompt
"Answer the number of elements in the list the reader makes from the text
\"(+ 1 2)\". Do not count them by eye — call the reader and ask the list."
        :template "(read-from-string \"(+ 1 2)\")"
        :check (eql value 3)
        :hint "`length' takes a list. `read-from-string' answers one.")
       (:prompt
"Answer the *first element* of that list. The check will insist it is a
symbol whose name is \"+\", so answering the character or the string will
not do."
        :template "(first (read-from-string \"(1 2 3)\"))"
        :check (and (symbolp value) (string= (symbol-name value) "+"))
        :hint "`first' — or `car', which is the same function under an older name.")
       (:prompt
"Two different texts, one object. Make =value= true by handing the reader
two spellings of the same symbol and asking whether the results are =eq= —
that is, the very same object in memory, not merely equal."
        :template "(eq (read-from-string \"foo\") (read-from-string \"bar\"))"
        :check (eq value t)
        :hint "The reader upcases. What else, besides \"foo\", reads as FOO?")))

     ;; -----------------------------------------------------------------------
     (:id "quote"
      :title "quote and eval, the two halves of a Lisp"
      :prose
"The evaluator's rule for a list is short: evaluate the elements after the
first, then apply the first to them. Its rule for a symbol is shorter: look
up its value. So =(+ 1 2)= is 3 and =x= is whatever x is bound to.

Which leaves a question the reader made unavoidable. If a program is a list,
and lists evaluate, how do you ever get your hands on a list *as a list*?

#+begin_src lisp
(quote (+ 1 2))
#+end_src

=quote= is a special operator, and the only thing it does is refuse to
evaluate its argument. It answers the list itself: three elements, the first
of them the symbol +, nothing added up. It is used constantly, so the reader
has an abbreviation for it — a leading apostrophe:

#+begin_src lisp
'(+ 1 2)          is exactly  (quote (+ 1 2))
#+end_src

The apostrophe is not syntax in the way a semicolon is syntax. It is a
reader macro: the reader sees ='=, reads the next object, and wraps it in a
two-element list. You can see this for yourself with =read-from-string=.

Going the other way is a function, not an operator:

#+begin_src lisp
(eval '(+ 1 2))   =>  3
#+end_src

=eval= is the evaluator, exposed. Most languages hide it or make it
frightening. Here it is ordinary — this editor's =C-c= is a call to it, the
REPL is a call to it in a loop, and the checker marking this exercise is
another. What makes it unremarkable is the reader: =eval= takes a *list*,
which you can build, take apart and inspect, rather than a string it would
have to parse.

The pair is worth holding in your head as one idea. =quote= moves down, from
code to data; =eval= moves up, from data to code. Lesson 12 is what happens
when you do both in the same breath.

A caution about =list= versus =quote=. =(list 1 (+ 1 1))= evaluates its
arguments and answers =(1 2)=. ='(1 (+ 1 1))= evaluates nothing and answers
a list whose second element is itself a list. They are not interchangeable,
and knowing which you want is most of knowing what quoting is for."
      :exercises
      ((:prompt
"Make =value= the three-element *list* whose elements are the symbol +, the
number 1 and the number 2 — not the number 3."
        :template "(+ 1 2)"
        :check (equal value '(+ 1 2))
        :hint "One character in front of the form is enough.")
       (:prompt
"Now go the other way. Starting from that list as data, produce the number
3 — without typing 3 anywhere."
        :template "'(+ 1 2)"
        :check (eql value 3)
        :hint "There is a function that takes a form and runs it.")
       (:prompt
"Build the list =(a 1 (b 2))=, where a and b are symbols, using =list= and
=quote= together. The point is to feel where evaluation stops: =list=
evaluates its arguments, so each symbol has to be protected on its own."
        :template "(list 'a 1 'b 2)"
        :check (equal value '(a 1 (b 2)))
        :hint "The third element is itself a list. `list' can make that too.")))

     ;; -----------------------------------------------------------------------
     (:id "cons"
      :title "Lists are a story told by cons cells"
      :prose
"There is no list type in Common Lisp. There is a two-slot object called a
cons cell, and there is the symbol NIL, and =list= is the name of a habit.

A cons cell holds two things. Historically they are called the car and the
cdr, after two fields of an IBM 704 instruction, and nobody has managed to
rename them in seventy years.

#+begin_src lisp
(cons 1 2)        =>  (1 . 2)
#+end_src

That is a cons cell holding 1 and 2. The dot in how it prints is the printer
telling you the truth: this is a pair, not a list.

A list is what you get when you agree to a convention — the cdr of each cell
holds the rest of the list, and the last cdr holds NIL:

#+begin_src lisp
(cons 1 (cons 2 (cons 3 nil)))   =>  (1 2 3)
#+end_src

=(1 2 3)= is *printed* differently from =(1 . (2 . (3 . nil)))= only because
the printer knows the convention and prefers the short spelling. They are
the same three cells.

Consequences, all of them things you will meet:

  - =first=/=car= is one memory access; =nth= walks. A list is a chain and
    indexing into it costs what walking costs.
  - =cons= is cheap and never copies. =(cons 0 xs)= shares every cell of xs.
  - =append= *does* copy — all but its last argument — because it has to
    rebuild the chain to point somewhere new.
  - The empty list and false are the same object. =nil= is NIL is =()= is
    false, and =(if '() 'yes 'no)= answers =no=. This is either elegant or
    a wart depending on the day.

A list whose last cdr is not NIL is called improper, and =(1 . 2)= is the
smallest one. Nothing forbids them; =length= simply cannot cope, and neither
can most of the standard sequence functions.

Recursion over a list is the shape you will write a hundred times:

#+begin_src lisp
(defun sum (xs)
  (if (null xs)
      0
      (+ (first xs) (sum (rest xs)))))
#+end_src

=null= asks whether you have reached the end. =rest= (or =cdr=) is
everything after the first cell."
      :exercises
      ((:prompt
"Make the list (1 2 3) using nothing but =cons= and =nil=. The template
below builds a one-element list; extend it."
        :template "(cons 1 nil)"
        :check (equal value '(1 2 3))
        :hint "Three cells. The cdr of each one is the rest of the list.")
       (:prompt
"Now make something that is *not* a list: a single cons cell whose car is 1
and whose cdr is 2."
        :template "(list 1 2)"
        :check (and (consp value) (eql (car value) 1) (eql (cdr value) 2)
                    (not (listp (cdr value))))
        :hint "One call, two arguments, and neither of them NIL.")
       (:prompt
"Write =my-length=, which counts the cells of a proper list. Any route
passes — recursion, =loop=, =reduce= — as long as it does not call
=length=. It must answer 0 for the empty list."
        :template "(defun my-length (xs) 1)"
        :check (and (= (my-length '()) 0)
                    (= (my-length '(a)) 1)
                    (= (my-length '(a b c d)) 4))
        :hint "`(null xs)' is the base case; `(rest xs)' is one cell shorter.")))

     ;; -----------------------------------------------------------------------
     (:id "let"
      :title "let, let*, and what a binding actually is"
      :prose
"=let= introduces names that exist for the length of its body and nowhere
else:

#+begin_src lisp
(let ((a 1)
      (b 2))
  (+ a b))        =>  3
#+end_src

Three things to notice, all of which catch people out once.

*The body is an implicit progn.* You may write as many forms as you like
inside; the value of the =let= is the value of the last one. The earlier
ones are there for their effects.

*The initialisers are evaluated in parallel.* All of them are computed
first, in the scope *outside* the =let=, and only then are the names bound.
So this does not work:

#+begin_src lisp
(let ((a 1)
      (b (+ a 1)))    ; a is not bound yet, out here
  (+ a b))
#+end_src

=let*= is the sequential version: each initialiser sees the names above it.
Prefer =let= when you can, because it says =these are independent=, and
reach for =let*= when one value is built from another. This editor's own
source uses both, deliberately — see the =let*= in =%repl-show=.

*Binding is not assignment.* A =let= inside another =let= that uses the
same name does not overwrite anything; it makes a new binding that shadows
the outer one for the length of the body, and the outer one is untouched
afterwards. That is what lexical scope means, and it is why lesson 7's
closures can work at all.

#+begin_src lisp
(let ((x 1))
  (list (let ((x 10)) x)      ; 10
        x))                    ; still 1
#+end_src

A binding with no initialiser is bound to NIL: =(let (a b) ...)=. And the
whole thing is one expression, so it can go wherever an expression can —
inside a function call, inside another =let=, as the body of a =defun=."
      :exercises
      ((:prompt
"The form below is broken: the second initialiser refers to the first, and
=let= will not have it. Fix it so the whole thing answers 3, changing as
little as possible."
        :template "(let ((a 1) (b (+ a 1))) (+ a b))"
        :check (eql value 3)
        :hint "One character.")
       (:prompt
"Shadowing. Bind x to 1, then in an inner scope bind it to 10, and answer
the cons cell (INNER . OUTER) — proving the outer binding survived."
        :template "(let ((x 1)) (cons x x))"
        :check (equal value '(10 . 1))
        :hint "The inner `let' is an expression, so it can be the first argument to `cons'.")
       (:prompt
"A =let= body is an implicit progn. Bind =total= to 0, add to it twice with
=incf= (which you will meet properly in lesson 11), and answer it — all
inside one =let=."
        :template "(let ((total 0)) total)"
        :check (and (integerp value) (> value 0))
        :hint "`(incf total 5)' adds 5 to total. Two of those, then `total'.")))

     ;; -----------------------------------------------------------------------
     (:id "defun"
      :title "defun, and the two namespaces"
      :prose
"#+begin_src lisp
(defun square (x)
  \"Answer X multiplied by itself.\"
  (* x x))
#+end_src

The name, the parameter list, an optional docstring, and a body that is an
implicit progn. The value of the last form is the value of the call; there is
no =return= keyword because there is nothing to return *from* — the body is
an expression like any other.

The docstring is not a comment. It is stored on the symbol and readable:

#+begin_src lisp
(documentation 'square 'function)
#+end_src

which is how =M-x= in this editor shows a description beside every command.
=runtime/modes/which-key.lisp= does exactly that, and the annotation you see
in the M-x list is one call to =documentation= per candidate.

There *is* a way out early, and it is a named exit rather than a keyword:

#+begin_src lisp
(defun clamp (x lo hi)
  (when (< x lo) (return-from clamp lo))
  (when (> x hi) (return-from clamp hi))
  x)
#+end_src

=defun= establishes a block with the function's own name, and =return-from=
leaves it. Anonymous functions get a block named NIL.

*The two namespaces.* This is the thing that separates Common Lisp from
Scheme, from Python, from almost everything, and it is worth ten minutes.

A symbol has a value cell and a function cell, and they are independent. So

#+begin_src lisp
(defun f (x) (* x x))
(let ((f 10))
  (list (f 3) f))     =>  (9 10)
#+end_src

=(f 3)= looks in the function cell. Bare =f= looks in the value cell. They
do not collide, which is why you may call a variable =list= or =car= without
breaking anything, and why nobody in Common Lisp writes =lst=.

The price is that passing a function as a value needs a way to say which
cell you mean. That is =function=, abbreviated =#'=:

#+begin_src lisp
(mapcar #'square '(1 2 3))   =>  (1 4 9)
#+end_src

and calling a function held in a variable needs =funcall=:

#+begin_src lisp
(let ((g #'square)) (funcall g 4))   =>  16
#+end_src

People call this Lisp-2. It costs you two extra characters in exchange for
never having to think about whether a local variable has shadowed a
function, which is a bargain almost everybody underestimates until they have
lived with both."
      :exercises
      ((:prompt
"Define =celsius->fahrenheit=, taking a temperature in Celsius and answering
it in Fahrenheit: multiply by 9/5 and add 32. The check calls it three
times, so a function that only gets one case right will not pass."
        :template "(defun celsius->fahrenheit (c) c)"
        :check (and (= (celsius->fahrenheit 0) 32)
                    (= (celsius->fahrenheit 100) 212)
                    (= (celsius->fahrenheit -40) -40))
        :hint "9/5 is a rational literal in Common Lisp and is exact — you do not need 1.8.")
       (:prompt
"Define =clamp= taking a number and a low and high bound, answering the
number held between them. Use =return-from= at least once, for the practice
— though nothing checks that you did."
        :template "(defun clamp (x lo hi) x)"
        :check (and (= (clamp 5 0 10) 5) (= (clamp -3 0 10) 0) (= (clamp 99 0 10) 10))
        :hint "`return-from' takes the block name — which is the function's name — then a value.")
       (:prompt
"Prove the two namespaces to yourself. Define a function =f= that doubles
its argument, then, in a =let= that binds the *variable* f to 10, answer the
two-element list ((f 4) f)."
        :template "(progn (defun f (x) (* 2 x)) (list (f 4) 4))"
        :check (equal value '(8 10))
        :hint "Nothing clever is needed. Write it as it reads and it works.")))

     ;; -----------------------------------------------------------------------
     (:id "lambda-lists"
      :title "&optional, &rest, &key — the parameter list is a little language"
      :prose
"Everything after the required parameters is a declaration about how calls
are allowed to be shaped.

*&optional* — trailing parameters that may be left out. NIL if they are,
unless you give a default:

#+begin_src lisp
(defun greet (name &optional (greeting \"hello\"))
  (concatenate 'string greeting \", \" name))
#+end_src

#+begin_src lisp
(greet \"ada\")           =>  \"hello, ada\"
(greet \"ada\" \"good day\") =>  \"good day, ada\"
#+end_src

You can also ask whether one was supplied, which distinguishes =not given=
from =given as NIL=:

#+begin_src lisp
(defun f (&optional (x nil x-p)) (list x x-p))
#+end_src

*&rest* — everything left over, as a list:

#+begin_src lisp
(defun total (&rest numbers) (apply #'+ numbers))
(total 1 2 3)   =>  6
(total)         =>  0
#+end_src

*&key* — parameters named at the call site rather than positioned:

#+begin_src lisp
(defun make-point (&key (x 0) (y 0)) (cons x y))
(make-point :y 3)        =>  (0 . 3)
(make-point :x 1 :y 2)   =>  (1 . 2)
#+end_src

Keywords are self-evaluating symbols living in their own package; lesson 14
comes back to what that means. For now: =:y= evaluates to itself, always,
and cannot be rebound, which is what makes it safe as a label.

*Which to use.* &optional past the second one is a trap: =(draw 3 nil nil t)=
is unreadable and adding a parameter in the middle breaks every caller. &key
is self-describing, order-free and extensible, and it is what the standard
library reaches for: =(sort xs #'< :key #'second)= and =(read in nil :eof)=.
The rule of thumb almost everybody converges on is: one optional is fine, two
is suspicious, three should have been keywords.

Note that this editor's own primitives follow it:
=(line-string &optional line)= has exactly one, and
=(make-marker &optional pos advance)= has two and says in its docstring that
it is a simplification."
      :exercises
      ((:prompt
"Define =greet= taking a name and an optional greeting that defaults to
\"hello\", answering the two joined by \", \"."
        :template "(defun greet (name &optional greeting) name)"
        :check (and (string= (greet "ada") "hello, ada")
                    (string= (greet "ada" "hi") "hi, ada"))
        :hint "A default goes in a list beside the parameter name.")
       (:prompt
"Define =make-point= taking keyword parameters x and y, each defaulting to
0, and answering them as a cons cell (X . Y)."
        :template "(defun make-point (x y) (cons x y))"
        :check (and (equal (make-point) '(0 . 0))
                    (equal (make-point :y 3) '(0 . 3))
                    (equal (make-point :x 1 :y 2) '(1 . 2)))
        :hint "&key, then each parameter with its default in a list.")
       (:prompt
"Define =total=, taking any number of numbers and answering their sum. It
must answer 0 when called with none."
        :template "(defun total (a b) (+ a b))"
        :check (and (= (total) 0) (= (total 7) 7) (= (total 1 2 3) 6))
        :hint "&rest gives you a list. `apply' calls a function with a list of arguments.")))

     ;; -----------------------------------------------------------------------
     (:id "lambda"
      :title "lambda, and what a closure is holding on to"
      :prose
"=lambda= is =defun= without the name:

#+begin_src lisp
(lambda (x) (* x x))
#+end_src

That is an expression whose value is a function. It can be called at once,
stored in a variable, passed as an argument, or answered from another
function. =#'(lambda ...)= also works and means the same thing; the bare
form has been legal since 1994 and is what most code writes now.

#+begin_src lisp
(funcall (lambda (x) (* x x)) 5)   =>  25
#+end_src

*A closure is a function plus the bindings it was made under.* Not the
values — the bindings. This distinction is the whole lesson:

#+begin_src lisp
(defun make-counter ()
  (let ((n 0))
    (lambda () (incf n))))
#+end_src

#+begin_src lisp
(let ((c (make-counter)))
  (list (funcall c) (funcall c) (funcall c)))   =>  (1 2 3)
#+end_src

=n= is gone from every other point of view — the =let= has returned — but
the closure holds the binding alive and keeps mutating the same one. Call
=make-counter= twice and you get two counters that do not interfere, because
each call to =make-counter= made a fresh binding.

That is also the answer to a bug you will write, probably this week:

#+begin_src lisp
(let ((fns nil))
  (dotimes (i 3) (push (lambda () i) fns))
  (mapcar #'funcall fns))       ; (3 3 3), not (2 1 0)
#+end_src

The standard leaves it *open* whether an iteration construct establishes a
fresh binding each time round or one binding it assigns to. So all three
closures may capture the same =i= and answer whatever it was left holding.
The fix is to make a binding of your own per iteration:

#+begin_src lisp
(dotimes (i 3)
  (let ((i i)) (push (lambda () i) fns)))
#+end_src

Worth doing once in the implementation you are standing in, because the
answer is not uniform even within it: in the ECL running this editor,
=dolist= binds afresh and =dotimes= does not. Which is the whole argument.
Code that relies on either is code that breaks when it moves.

This is not a hypothetical. =crates/lisp/src/shim.c= has a comment about it
where it builds one closure per reader, and =runtime/modes/modes.lisp= has
another where it wraps the setting primitives. Both carry the same comment —
LET so the closure captures its own binding rather than the loop's — and both
are there because the bug happened."
      :exercises
      ((:prompt
"Make =value= a function of one argument that adds 5 to it. Do not use
=defun= — the answer is an expression whose value is a function."
        :template "(lambda (x) x)"
        :check (and (functionp value) (= (funcall value 1) 6) (= (funcall value 0) 5))
        :hint "The template is already the right shape. Change the body.")
       (:prompt
"Define =make-counter=, which takes no arguments and answers a function that
answers 1 the first time it is called, 2 the second time, and so on. Two
counters made separately must not interfere."
        :template "(defun make-counter () (lambda () 1))"
        :check (let ((c (make-counter)) (d (make-counter)))
                 (and (= (funcall c) 1) (= (funcall c) 2)
                      (= (funcall c) 3) (= (funcall d) 1)))
        :hint "A `let' around the `lambda', and `incf' inside it.")
       (:prompt
"The loop bug, on purpose. Make =value= a list of three functions of no
arguments, the first answering 0, the second 1, the third 2 — built with
=dotimes= rather than written out. The template is the bug; fix it."
        :template "(let ((fns nil))
  (dotimes (i 3) (push (lambda () i) fns))
  (reverse fns))"
        :check (equal (mapcar #'funcall value) '(0 1 2))
        :hint "Give each iteration a binding of its own with an inner `let'.")))

     ;; -----------------------------------------------------------------------
     (:id "higher-order"
      :title "Functions as arguments"
      :prose
"Once a function is a value, a great deal of code that would otherwise be a
loop becomes a call.

#+begin_src lisp
(mapcar #'square '(1 2 3 4))        =>  (1 4 9 16)
(remove-if-not #'evenp '(1 2 3 4))  =>  (2 4)
(reduce #'+ '(1 2 3 4))             =>  10
(sort (copy-list xs) #'< :key #'second)
#+end_src

=mapcar= walks one list and collects; given several lists it walks them
together and stops at the shortest. =remove-if= and =remove-if-not= filter.
=reduce= folds a two-argument function through a sequence, with
=:initial-value= when the empty case needs an answer other than what the
function gives for no arguments.

=:key= appears all over the standard library and is worth learning as a
concept rather than as a parameter. It says =before comparing, apply this= —
so =(find \"cl\" stages :key #'first :test #'string=)= looks inside each
element rather than at it. The tutor you are reading finds a lesson exactly
that way.

=apply= calls a function with a list of arguments, spread out:

#+begin_src lisp
(apply #'+ '(1 2 3))   =>  6
#+end_src

=funcall= is the same thing with the arguments written out. Both exist
because of the two namespaces from lesson 5: a function held in a variable
is data, and data needs a verb to become a call.

A note on =#'= versus ='=. =#'foo= means the function named foo, resolved
now, lexically. ='foo= is just the symbol, and =funcall= will accept it and
look it up in the global function cell at call time. Prefer =#'=: it is
checked earlier and it respects =flet= and =labels=.

=complement=, =every= and =some= round out the set:

#+begin_src lisp
(every #'numberp '(1 2 3))      =>  T
(some #'stringp '(1 \"a\" 2))     =>  T
(remove-if (complement #'evenp) '(1 2 3 4))   =>  (2 4)
#+end_src"
      :exercises
      ((:prompt
"Answer the list of the squares of 1 through 5, using =mapcar= and a list
literal. You will do this again two more ways in the next lesson."
        :template "(mapcar #'identity '(1 2 3 4 5))"
        :check (equal value '(1 4 9 16 25))
        :hint "A `lambda' is fine as the function argument.")
       (:prompt
"Answer the sum of the integers 1 through 10 with =reduce=."
        :template "(reduce #'max '(1 2 3 4 5 6 7 8 9 10))"
        :check (eql value 55)
        :hint "The function you fold with is the one that adds two numbers.")
       (:prompt
"From the list (1 2 3 4 5 6 7), answer only the even numbers, in order,
using one of the filtering functions."
        :template "(remove-if #'evenp '(1 2 3 4 5 6 7))"
        :check (equal value '(2 4 6))
        :hint "The template removes exactly the ones you want to keep.")))

     ;; -----------------------------------------------------------------------
     (:id "iteration"
      :title "loop, dolist, dotimes — and when each one is right"
      :prose
"Common Lisp has more iteration constructs than it needs, which is what
happens when a language is standardised out of several dialects that each
had a favourite. All of them are worth knowing because all of them are in
code you will read.

*dolist and dotimes* are the small, honest ones:

#+begin_src lisp
(dolist (x '(a b c)) (print x))
(dotimes (i 3) (print i))          ; 0, 1, 2
#+end_src

Both answer NIL unless you give a third element in the head —
=(dolist (x xs result) ...)= — which is a wart nobody uses. They are for side
effects.

*loop* is a language of its own, embedded:

#+begin_src lisp
(loop for i from 1 to 5 collect (* i i))      =>  (1 4 9 16 25)
(loop for x in xs when (evenp x) sum x)
(loop for k being the hash-keys of h collect k)
(loop repeat 3 do (print \"hi\"))
#+end_src

It is not made of parentheses, which offends a certain kind of Lisp
programmer permanently, and it is extraordinarily convenient, which is why
they lost. Note the =for ... in= versus =for ... on=: =in= walks the
elements, =on= walks the cells, so =on= gives you the tails.

=loop= has a simple form too, with no keywords at all: =(loop (do-something))=
is an infinite loop, exited with =return=. Every REPL in existence is one,
including this editor's, and =(loop)= with an empty body is the classic way
to wedge an image — which is why Stage 1 of this tutorial marks your answers
in a *child* process with a stopwatch on it. Try it, if you like. It comes
back as a failed exercise rather than a dead editor.

*mapcar* you met last lesson, and it is the third way to write the same
thing. The three exist at different altitudes:

  - =mapcar= says what the result *is* — one output per input.
  - =loop ... collect= says the same, with room to filter and count.
  - =dolist= says how it is *built*, and is the one to reach for when the
    work is a side effect rather than a value.

Reach for the highest one that fits. This codebase does: =runtime/modes/=
uses =mapcar= where the shape is one-to-one, =loop ... collect= where it
filters, and =dolist= where it is issuing commands to the editor."
      :exercises
      ((:prompt
"The same list of squares as last lesson — 1 through 5 — but with =loop=
this time. One expression, no helper function."
        :template "(loop for i from 1 to 5 collect i)"
        :check (equal value '(1 4 9 16 25))
        :hint "`collect' takes an expression, not just a variable.")
       (:prompt
"With =dolist= and a =let=, build the list (3 2 1) by walking over '(1 2 3)
and pushing each element onto an accumulator. =push= adds to the front,
which is why this comes out reversed — that is the point."
        :template "(let ((out nil)) (dolist (x '(1 2 3)) x) out)"
        :check (equal value '(3 2 1))
        :hint "`(push x out)' inside the `dolist' body.")
       (:prompt
"With one =loop=, answer the sum of the even numbers between 1 and 20
inclusive."
        :template "(loop for i from 1 to 20 sum i)"
        :check (eql value 110)
        :hint "`when' can go between `for' and `sum'.")))

     ;; -----------------------------------------------------------------------
     (:id "values"
      :title "More than one answer"
      :prose
"A Common Lisp form can answer several values at once. Not a list of them —
several values, which is a different thing, and which costs nothing when the
caller only wants the first.

#+begin_src lisp
(floor 17 5)   =>  3 and 2
#+end_src

=floor= answers the quotient and the remainder. In a context that wants one
value, the extra ones are dropped silently:

#+begin_src lisp
(+ (floor 17 5) 0)   =>  3
(list (floor 17 5))  =>  (3)
#+end_src

To make several, use =values=:

#+begin_src lisp
(defun divmod (a b)
  (values (floor a b) (mod a b)))
#+end_src

To take them, =multiple-value-bind=:

#+begin_src lisp
(multiple-value-bind (q r) (divmod 17 5)
  (list q r))        =>  (3 2)
#+end_src

or =nth-value= when you want exactly one:

#+begin_src lisp
(nth-value 1 (floor 17 5))   =>  2
#+end_src

=(values)= answers *no* values at all, which in a single-value context reads
as NIL. And =multiple-value-list= collects them into a list when you would
rather have one.

Why this instead of answering a list? Because the common case is that the
caller wants the primary value and should pay nothing for the others. It is
what makes =gethash= pleasant:

#+begin_src lisp
(gethash :missing table)   =>  NIL and NIL
(gethash :present table)   =>  NIL and T
#+end_src

The second value distinguishes =absent= from =present, holding NIL=, and
every caller that does not care never notices it is there. The same trick
runs through the standard library — =read-from-string= answers where it
stopped reading, =intern= answers whether the symbol already existed — and
through this editor: =%eval-source= in =runtime/modes/repl.lisp= answers a
value and a condition report, so one call tells the REPL both what happened
and whether it worked."
      :exercises
      ((:prompt
"Define =divmod=, taking two integers and answering *two values*: the
quotient and the remainder. The check uses =multiple-value-bind=, so
answering a list will not pass."
        :template "(defun divmod (a b) (list (floor a b) (mod a b)))"
        :check (multiple-value-bind (q r) (divmod 17 5)
                 (and (eql q 3) (eql r 2)))
        :hint "`floor' already answers both. There is a way to pass them straight through.")
       (:prompt
"Answer the *second* value of =(floor 17 5)= — the remainder — without
calling =mod= and without =multiple-value-bind=."
        :template "(floor 17 5)"
        :check (eql value 2)
        :hint "There is a function that takes an index and a form.")
       (:prompt
"Show that extra values are dropped. Make =value= the one-element list
containing only the primary value of =(floor 17 5)=."
        :template "(multiple-value-list (floor 17 5))"
        :check (equal value '(3))
        :hint "`list' takes one value from each argument form. That is all you need.")))

     ;; -----------------------------------------------------------------------
     (:id "setf"
      :title "setf, and the idea of a place"
      :prose
"=setq= assigns to a variable. =setf= assigns to a *place*, and a place is
anything you can read with an accessor that has an agreed way to write it:

#+begin_src lisp
(setf x 10)                 ; a variable
(setf (car pair) 'new)      ; the car of a cons
(setf (gethash :k table) 1) ; an entry in a hash table
(setf (aref v 0) 'first)    ; an element of a vector
(setf (symbol-function 'f) #'square)   ; a function definition
#+end_src

The rule is uniform and worth stating plainly: for almost every accessor
=(foo x)= in the standard, =(setf (foo x) v)= does the corresponding update.
You do not learn a setter per getter; you learn the getter and put =setf= in
front of it.

=setf= is a macro (lesson 12), and what it expands into is the update code
for whichever accessor it found. You can teach it about your own accessors
by defining a method on =(setf your-accessor)=.

The family built on top of it is where most of the mileage is:

#+begin_src lisp
(incf n)        (incf n 5)        (decf n)
(push x list)   (pop list)        (pushnew x list :test #'equal)
(rotatef a b)   (shiftf a b 0)
#+end_src

All of them take *places*, not just variables, and that is the payoff:
=(incf (gethash :hits table) 1)= and =(push item (gethash :k table))= both
work, and neither needed anyone to write a special version. This tutor's own
=%tutor-report= does =(setf (cdr old) text)= on an alist entry for exactly
that reason.

Two cautions. First, =setf= on a =let=-bound name mutates the binding, and
if a closure is holding that binding it will see the change — which is what
makes =make-counter= work and what makes shared mutable state a hazard, in
the same breath. Second, =push= and =setf= on a *literal* list — one that
came from =quote= — is undefined behaviour, because a quoted list may be
shared with the source of your program. Build it with =list= or =copy-list=
first if you mean to change it."
      :exercises
      ((:prompt
"Make a cons cell holding 1 and 2, change its car to 99 with =setf=, and
answer the cell. Build the cell with =cons= rather than quoting one."
        :template "(let ((pair (cons 1 2))) pair)"
        :check (equal value '(99 . 2))
        :hint "`(car pair)' is the place.")
       (:prompt
"Make a hash table, put 1 under the key :a and 2 under :b, and answer the
table. The check reads both keys back out."
        :template "(let ((h (make-hash-table))) h)"
        :check (and (hash-table-p value)
                    (eql (gethash :a value) 1)
                    (eql (gethash :b value) 2))
        :hint "`(gethash :a h)' is the place, and `setf' knows what to do with it.")
       (:prompt
"=incf= takes a place, not just a variable. Make a cons cell holding 0 and
0, add 5 to its cdr with one =incf=, and answer the cell."
        :template "(let ((p (cons 0 0))) p)"
        :check (equal value '(0 . 5))
        :hint "`(incf (cdr p) 5)'.")))

     ;; -----------------------------------------------------------------------
     (:id "macros"
      :title "defmacro: programs that write programs"
      :prose
"Everything so far has been a language with unusually few rules. This is the
lesson where the few rules pay for themselves.

A macro is a function from code to code. It runs *before* evaluation, its
arguments are the unevaluated forms as the reader made them, and whatever it
answers is what gets evaluated in its place.

#+begin_src lisp
(defmacro my-unless (test &body body)
  `(if ,test nil (progn ,@body)))
#+end_src

The backquote is a template. Inside it, everything is quoted except what is
marked: =,x= drops in the value of x, and =,@xs= splices a list in. So

#+begin_src lisp
(my-unless done (print \"still going\"))
#+end_src

expands to

#+begin_src lisp
(if done nil (progn (print \"still going\")))
#+end_src

which is then evaluated normally. You can watch it happen:

#+begin_src lisp
(macroexpand-1 '(my-unless a b))
#+end_src

*Why this cannot be a function.* A function receives values, so a function
version of =my-unless= would have evaluated =(print \"still going\")= before
being called, and the whole point is not to. Every construct that controls
*whether* or *how often* something is evaluated has to be a macro or a
special operator. =and=, =or=, =when=, =unless=, =dolist=, =loop=,
=with-open-file= — all macros, all in the standard, none of them privileged.
You can write things of exactly the same kind, and that is what people mean
when they say Lisp is extensible in a way other languages are not: there is
no line between the constructs the language shipped with and the ones you
add.

This editor is built on that. =define-derived-mode= in
=runtime/modes/modes.lisp= is a macro that writes two =defun= forms, a
registration call and a hash-table update; =with-current-buffer= and
=save-excursion= are macros in =crates/lisp/src/shim.c= that wrap a body in
an =unwind-protect=. None of them could be functions.

*Variable capture.* The classic hazard:

#+begin_src lisp
(defmacro swap (a b)
  `(let ((tmp ,a)) (setf ,a ,b) (setf ,b tmp)))    ; broken
#+end_src

Call it as =(swap x tmp)= and the expansion binds =tmp= over the caller's
=tmp=. The fix is a symbol nobody can have typed:

#+begin_src lisp
(defmacro swap (a b)
  (let ((tmp (gensym)))
    `(let ((,tmp ,a)) (setf ,a ,b) (setf ,b ,tmp))))
#+end_src

=gensym= answers a fresh uninterned symbol every time. =shim.c= does exactly
this — =(let ((old (gensym)) (want (gensym))) ...)= — in both of its macros,
and lesson 14 explains what =uninterned= means.

*When not to.* A macro is harder to compose than a function: you cannot
=mapcar= it, you cannot pass it, and it complicates every tool that reads
your code. Write the function first. Write the macro only when the thing you
need is control over evaluation."
      :exercises
      ((:prompt
"Write =my-unless= as a macro: =(my-unless TEST . BODY)= evaluates the body
only when TEST is false, and answers NIL otherwise. Use =&body= for the
body."
        :template "(defmacro my-unless (test &body body) (list 'if test nil))"
        :check (and (eql (my-unless nil 41 42) 42)
                    (null (my-unless t 41 42)))
        :hint "A backquote template with `,test' and `,@body' in it.")
       (:prompt
"Write =swap=, a macro taking two places and exchanging their contents. Use
=gensym= for the temporary, so that =(swap a tmp)= would still work."
        :template "(defmacro swap (a b) `(let ((tmp ,a)) (setf ,a ,b) (setf ,b tmp)))"
        :check (let ((a 1) (b 2) (tmp 3))
                 (swap a b)
                 (swap a tmp)
                 (and (= a 3) (= b 1) (= tmp 2)))
        :hint "Bind the gensym outside the backquote, then comma it in wherever the name appears.")
       (:prompt
"Show that a macro really does control evaluation. Write =my-and2=, taking
two forms, which evaluates the second only when the first is true, and
answers the second's value (or NIL)."
        :template "(defun my-and2 (a b) (if a b nil))"
        :check (let ((hits 0))
                 (flet ((bump () (incf hits) t))
                   (my-and2 nil (bump))
                   (and (zerop hits)
                        (my-and2 t (bump))
                        (= hits 1))))
        :hint "The template is a function, so both arguments are evaluated before it is even called.")))

     ;; -----------------------------------------------------------------------
     (:id "conditions"
      :title "Conditions, which are not exceptions"
      :prose
"Signalling in Common Lisp looks like exception handling and is a larger
system than that. The difference is one sentence:
*the stack is not unwound before the handler runs.*

The familiar half first:

#+begin_src lisp
(handler-case (/ 1 0)
  (division-by-zero () :oops))       =>  :OOPS
#+end_src

=handler-case= unwinds and then runs the clause, exactly like =try=/=catch=.
Clauses are matched by condition *type*, and the types form a hierarchy:
=division-by-zero= is an =arithmetic-error= is an =error= is a
=serious-condition= is a =condition=. Catching a broad type catches
everything under it.

That hierarchy is why this editor's REPL catches =serious-condition= rather
than =error=: a stack overflow is not an =error= in Common Lisp, and it is
precisely the condition a beginner's recursion produces. =repl.lisp= says so
in a comment, and this tutor's checker inherits it.

Two more things you will want:

#+begin_src lisp
(ignore-errors (parse-integer \"nope\"))     => NIL and the condition
(unwind-protect (risky) (always-clean-up))
#+end_src

=unwind-protect= runs its cleanup however the body leaves — normally, by a
signal, or by a =return-from=. It is =finally=, and =with-open-file= and
=with-current-buffer= are both built out of it.

Your own conditions are classes:

#+begin_src lisp
(define-condition too-big (error)
  ((amount :initarg :amount :reader too-big-amount))
  (:report (lambda (c s) (format s \"~a is too big\" (too-big-amount c)))))
#+end_src

#+begin_src lisp
(error 'too-big :amount 99)
#+end_src

*The half that is not exceptions.* =handler-bind= installs a handler that
runs *at the point of the signal*, with everything below it still on the
stack. From there it may decline (by returning normally, letting the search
continue), or it may invoke a restart — a named way to continue that the
signalling code offered:

#+begin_src lisp
(handler-bind ((too-big (lambda (c) (invoke-restart 'use-value 10))))
  (with-restarts ...))
#+end_src

That is how a Lisp debugger can offer you =retry=, =use this value instead=
and =skip this file= rather than only a stack trace: the choices are
established by the code that knew what the alternatives were, and the
handler picks one without the stack having been thrown away.

You will not need restarts today. Knowing they exist is the point, because
it explains why the vocabulary is =condition= and =signal= rather than
=exception= and =throw=: not every condition is an error, and not every
signal is a request to stop."
      :exercises
      ((:prompt
"Answer the keyword :oops when the division below signals, using
=handler-case=. Catch =division-by-zero= specifically, not =error=."
        :template "(handler-case (/ 1 0) (error () :caught))"
        :check (eq value :oops)
        :hint "The clause is (TYPE (VARS) FORMS...); an empty variable list is fine.")
       (:prompt
"Define a condition =too-big= inheriting from =error=, with one slot
=amount= that has an =:initarg= of :amount and a reader =too-big-amount=.
Then signal it with an amount of 99 and, in a =handler-case=, answer the
amount you get back out of it."
        :template "(progn
  (define-condition too-big (error) ())
  (handler-case (error 'too-big) (too-big (c) 0)))"
        :check (eql value 99)
        :hint "The clause variable is bound to the condition object; hand it to your reader.")
       (:prompt
"Show that =unwind-protect= cleans up even when the body does not finish.
Keep a log: push :cleaned from a cleanup form, push :caught from a
=handler-case= clause, and answer the two *in the order they happened*. Which
order that is, is the thing worth finding out."
        :template "(let ((log nil))
  (handler-case (progn (error \"boom\"))
    (error () (push :caught log)))
  (reverse log))"
        :check (equal value '(:cleaned :caught))
        :hint "Wrap the erroring form in `unwind-protect'. `push' adds to the front, so `reverse' at the end puts the log back into chronological order.")))

     ;; -----------------------------------------------------------------------
     (:id "packages"
      :title "Packages, and what a symbol really is"
      :prose
"A symbol is an object with a name, and usually a home package, and up to
four things hanging off it: a value, a function, a property list and a type
definition. =foo= the variable and =foo= the function are two cells of one
object, which is lesson 5's Lisp-2 seen from the other side.

A package is a table from names to symbols. =intern= looks a name up and
creates the symbol if it is not there; =find-symbol= looks it up and answers
NIL if it is not, plus a second value saying how it was found.

#+begin_src lisp
(symbol-package 'car)   =>  #<package COMMON-LISP>
(find-symbol \"CAR\" \"COMMON-LISP\")   =>  CAR and :EXTERNAL
#+end_src

Note the capitals again. The reader upcased =car= before interning it, so
the name stored is \"CAR\", and =find-symbol= — which takes a *string* and
does no reading — has to be given it that way. This is the single most
common beginner's confusion in the whole language, and it is also why
=runtime/modes/modes.lisp= writes
=(find-symbol (string-upcase name) :zemacs)= every time it looks a hook up.

Packages are named in three ways at a call site:

#+begin_example
zemacs:message      an external symbol — part of the interface
zemacs::%repl-eval  an internal one — the extra colon is the warning
message             whatever `*package*' resolves it to
#+end_example

=defpackage= declares one and =in-package= switches =*package*= for the rest
of the file. =:use= inherits another package's external symbols, which is
why every file here says =(in-package :zemacs)= at the top and then writes
=format=, =let= and =message= without any prefix at all: ZEMACS is defined
with =(:use \"CL\")=, so all of Common Lisp is inherited, and the editor's
own primitives are ZEMACS's own.

*Keywords* live in the KEYWORD package, are external, evaluate to
themselves and are interned by the reader the moment it sees the leading
colon. So =:foo= is always the same object everywhere in the image, which is
what makes keywords safe as labels in plists and =&key= parameters — nothing
can rebind one, and two libraries using =:name= are using the same object
rather than colliding.

*Uninterned symbols* belong to no package. =(gensym)= makes one, and the
reader writes them =#:foo=. Nobody can type a reference to one, which is
exactly the property lesson 12 needed."
      :exercises
      ((:prompt
"Answer the *name* of the package that the symbol =car= calls home, as a
string."
        :template "(symbol-package 'car)"
        :check (and (stringp value) (string= value "COMMON-LISP"))
        :hint "`package-name' turns a package object into its name.")
       (:prompt
"Two spellings of one keyword. Make =value= true by showing that the
keyword :foo is =eq= to the symbol you get by interning the string \"FOO\"
in the KEYWORD package."
        :template "(eq :foo (intern \"foo\" \"KEYWORD\"))"
        :check (eq value t)
        :hint "The reader upcased :foo before interning it. The template did not upcase its string.")
       (:prompt
"The reader upcases, but =intern= does not. Make =value= a symbol whose
=symbol-name= is the lowercase string \"foo\"."
        :template "'foo"
        :check (and (symbolp value) (string= (symbol-name value) "foo"))
        :hint "`intern' takes the name exactly as you give it.")))

     ;; -----------------------------------------------------------------------
     (:id "clos"
      :title "Generic functions, and why they are not methods on objects"
      :prose
"CLOS is the object system, and it is arranged the other way round from
almost every other one you have met.

A class holds slots and says nothing about behaviour:

#+begin_src lisp
(defclass point ()
  ((x :initarg :x :initform 0 :accessor point-x)
   (y :initarg :y :initform 0 :accessor point-y)))
#+end_src

#+begin_src lisp
(make-instance 'point :x 3 :y 4)
#+end_src

=:initarg= names the keyword =make-instance= accepts, =:initform= is the
default, and =:accessor= defines a reader *and* a =setf= place — so
=(setf (point-x p) 10)= works, for the reason lesson 11 gave.

Behaviour lives in generic functions, which are not owned by anybody:

#+begin_src lisp
(defgeneric area (shape))
#+end_src

#+begin_src lisp
(defmethod area ((s square))  (* (side s) (side s)))
(defmethod area ((c circle))  (* pi (radius c) (radius c)))
#+end_src

=(area x)= looks at the *arguments* and picks the applicable methods. Two
consequences follow, and they are the reason CLOS is worth the lesson:

*There is no message send.* =area= is an ordinary function you can pass to
=mapcar=. Adding a method to somebody else's generic function for your own
class does not require touching their code, and adding a method to your own
generic function for *their* class does not either.

*Dispatch is on every specialised argument, not just the first.*

#+begin_src lisp
(defmethod collide ((a asteroid) (b ship)) :ship-damaged)
(defmethod collide ((a ship) (b asteroid)) :also-damaged)
#+end_src

Multiple dispatch is not expressible in a language where methods belong to
one receiver; there you write a visitor and pretend. Here it is a parameter
list.

Method combination is the third piece. Besides primary methods there are
qualifiers:

#+begin_src lisp
(defmethod describe-it :before ((x point)) (print \"about to\"))
(defmethod describe-it :after  ((x point)) (print \"just did\"))
(defmethod describe-it :around ((x point)) (call-next-method))
#+end_src

and =call-next-method= inside a primary method runs the next most specific
one — which is how a subclass extends rather than replaces its parent's
behaviour, with the parent's method never having been written to expect it.

This editor does not use CLOS, and it is worth saying why rather than
leaving you to wonder: everything crossing the boundary to Rust is a number,
a string or a list, the mode system dispatches on a string it got from the
editor, and a =defstruct= would have bought nothing that a plist does not.
Knowing CLOS is still not optional — the moment you load a library, you are
in it."
      :exercises
      ((:prompt
"Define a class =point= with slots x and y, each with an =:initarg= of :x
and :y, an =:initform= of 0, and accessors =point-x= and =point-y=. Then
answer an instance made with x = 3 and y = 4."
        :template "(progn
  (defclass point () ((x) (y)))
  (make-instance 'point))"
        :check (and (= (point-x value) 3) (= (point-y value) 4)
                    (= (point-x (make-instance 'point)) 0))
        :hint "Each slot is (NAME :initarg ... :initform ... :accessor ...).")
       (:prompt
"Define a generic function =area= and two classes, =square= with a slot
=side= and =circle= with a slot =radius= (both with an =:initarg= and a
reader), and a method of =area= for each. A square of side 3 has area 9; a
circle of radius 1 has area pi."
        :template "(progn
  (defclass square () ((side :initarg :side :reader side)))
  (defgeneric area (shape)))"
        :check (and (= (area (make-instance 'square :side 3)) 9)
                    (< (abs (- (area (make-instance 'circle :radius 1)) pi)) 1/1000))
        :hint "A method's parameter list specialises with ((s square)) — the class name beside the parameter.")
       (:prompt
"Multiple dispatch. Define classes =rock= and =paper=, a generic function
=beats=, and two methods: =(beats rock paper)= answers :paper and
=(beats paper rock)= answers :paper as well — the same winner, reached by
two different method selections."
        :template "(progn
  (defclass rock () ())
  (defclass paper () ())
  (defgeneric beats (a b))
  (defmethod beats ((a rock) (b paper)) :rock))"
        :check (and (eq (beats (make-instance 'rock) (make-instance 'paper)) :paper)
                    (eq (beats (make-instance 'paper) (make-instance 'rock)) :paper))
        :hint "Two methods, differing only in the order of their two specialisers.")))))
  "Stage 1 — the language. Checked in a child `ecl', so a runaway answer costs
you the exercise and nothing else.")

;;; ---------------------------------------------------------------------------
;;; Stage 2 — the editor's own Lisp.
;;;
;;; Every name below was taken from the source rather than remembered: the
;;; reader and writer lists at the head of `runtime/init.lisp', the primitive
;;; definitions in `crates/lisp/src/shim.c', and `docs/reference.org'. If a
;;; lesson here disagrees with the editor, the editor is right and this is a
;;; bug.
;;;
;;; These run in the live image, in the `*tutor-scratch*' buffer, and the checks
;;; observe the editor afterwards through its own readers — which is exactly
;;; what a plugin does, and the only honest way to mark an exercise about
;;; controlling an editor.

(defparameter *tutor-stage-2*
  '(:id "api"
    :title "Stage 2 — driving this editor from Lisp"
    :check :image
    :lessons

    (;; -----------------------------------------------------------------------
     (:id "image"
      :title "You are inside the image"
      :prose
"This editor is a Common Lisp image with a text editor attached, and not the
other way round. =runtime/init.lisp= was LOADed into it at startup; every key
binding either names a built-in verb or calls a function in this image; =C-c=
evaluates the buffer you are looking at. There is no restart and no reload
step, because there is nothing to restart — a =defun= you evaluate now
replaces the old definition for every future call, including calls from
Rust.

From here on your answers run *here*, in that image. Stage 1's ran in a
child process with a stopwatch on them; these cannot, because the whole
point of an exercise like =(set-font-size 30)= is that the editor in front
of you gets bigger, and a child process would resize nothing.

Say the consequence out loud, once, because it is the only thing in this
stage that can hurt you:
*a Stage 2 answer containing (loop) will hang the image.*
The Lisp thread evaluates one queued form at a time and nothing
preempts it. The editor keeps drawing, keeps taking your keystrokes and can
still save your files — that much the threading design guarantees — but no
further Lisp runs until you restart. =docs/boundary.org= lists this as a
ceiling of the design. Now you know where it is.

Where your answers go: exercises are evaluated with the =*tutor-scratch*=
buffer live, in the pane beside this one. That is so an exercise about
=insert= has somewhere to insert. Anything that is *not* about the buffer —
the font size, a key binding, a mode — affects the whole editor as usual.
=C-c C-w= puts the scratch buffer back if you rearrange it too far.

Three things you can use from anywhere:

#+begin_src lisp
(message \"...\")        the status line
(messages)             the whole log, oldest first
(messages 5)           the last five
#+end_src

=M-x= is the one part of the extension mechanism that needs a registration
step, because it has to know the names before you type them. A zero-argument
function becomes a command when =refresh-commands= publishes it — that call
at the foot of =init.lisp= is why every function in your config is already
there."
      :exercises
      ((:prompt
"Put something in the status line. The check looks in the message log for
the word =hello=, so any message containing it will do."
        :template "(message \"nothing to see here\")"
        :check (find-if (lambda (m) (search "hello" m)) (messages 5))
        :hint "`message' takes a string. `(format nil ...)' makes one if you want the practice.")
       (:prompt
"Define a zero-argument function =tutor-hello= that messages something, then
make =M-x= able to find it. Defining it is not enough — the M-x list is
published, and there is a command in =init.lisp= that publishes it."
        :template "(defun tutor-hello () (message \"hi\"))"
        :check (and (fboundp (find-symbol "TUTOR-HELLO" :zemacs))
                    (find-if (lambda (c) (search "tutor-hello" c)) (command-list)))
        :hint "`refresh-commands' walks the ZEMACS package and republishes every zero-argument function.")
       (:prompt
"Make the editor bigger, for real. Set the font size to 30 and answer what
the editor then reports it as — the setter answers nothing useful, so you
will need the reader too. Press =M-0= afterwards to put it back."
        :template "(set-font-size 30)"
        :check (and (numberp value) (= value 30) (= (font-size) 30))
        :hint "`font-size' is a reader: no arguments, answers the live value.")))

     ;; -----------------------------------------------------------------------
     (:id "readers"
      :title "Readers: asking the editor questions"
      :prose
"A reader takes no arguments and answers something about the live buffer or
the live editor. There is no =current-buffer= object to pass around; the
editor has one live buffer and a reader asks about that one. The full list is
at the head of =runtime/init.lisp=; the ones you will use constantly:

#+begin_src lisp
(point) (point-min) (point-max) (buffer-size)
(line-number) (column)             1-based line, 0-based column
(line-count) (line-start) (line-end)
(buffer-string) (line-string) (buffer-substring beg end)
(buffer-name) (buffer-file-name)   the latter NIL for a generated buffer
(major-mode) (minor-modes) (evil-state)
(region)                           (BEG . END), or NIL if nothing is
                                   selected
(region-beginning) (region-end) (region-text)
(buffer-names) (buffer-list) (window-list) (window-id)
#+end_src

*Offsets are characters, counted from 0.* Not bytes and not lines. That
matters the moment a buffer contains anything outside ASCII, and it is why
=point= and =buffer-substring= agree with each other without anybody having
to convert.

=line-start=, =line-end= and =line-string= take an optional 1-based line
number, and with no argument mean the line point is on. So
=(buffer-substring (line-start) (line-end))= is =(line-string)= the long
way round, and that is the first exercise.

*NIL is the answer for \"there isn't one\".* =(region)= is NIL with nothing
selected, =(buffer-file-name)= is NIL for a buffer with no file. So
=(when (region) ...)= is the idiom throughout, and =region-text= answers NIL
rather than the empty string, which is a distinction worth keeping.

A reader is a noun, not a command, which is why none of them appears in
=M-x= — =init.lisp= takes the whole set out of the list by reading
=*readers*=, the same list the shim interned them from.

=M-x lisp-version= is a reader dressed as a command, and worth running once:
it counts the symbols in the ZEMACS package, which is the package everything
in this stage lives in."
      :exercises
      ((:prompt
"Answer the text of the line point is on — without calling =line-string=.
Two readers and one substring."
        :template "(buffer-string)"
        :check (equal value (line-string))
        :hint "`line-start' and `line-end' with no arguments mean this line.")
       (:prompt
"Answer the cons cell (A . B) where A is the buffer's size and B is the
same number worked out from the two ends of the buffer — proving to yourself
that offsets start at 0."
        :template "(cons (buffer-size) 0)"
        :check (and (consp value) (= (car value) (cdr value) (buffer-size)))
        :hint "`point-max' minus `point-min'.")
       (:prompt
"The NIL idiom. Answer the selected text if there is a selection, and the
string \"nothing selected\" if there is not. There is no selection right now,
so the check expects the second."
        :template "(region-text)"
        :check (if (region) (equal value (region-text)) (equal value "nothing selected"))
        :hint "`(if (region) ... ...)'. `region' is the reader that answers NIL.")))

     ;; -----------------------------------------------------------------------
     (:id "writers"
      :title "Writers, and why one call beats two"
      :prose
"#+begin_src lisp
(insert text)                 at point, moving it
(insert-at pos text)          without moving point
(goto-char n)
(delete-region beg end)
(replace-region beg end text) delete and insert, atomically
(replace-all from to)         answers how many
(undo) (redo)
#+end_src

Each of these is one undo step, and =u= takes the whole of it back out.

The interesting one is =replace-region=, and the reason is the threading
design. In Emacs, Lisp *is* the editor: while your function runs, nothing
else happens, so a sequence of edits is atomic for free and a slow function
freezes the display. Here Lisp runs on its own thread and never blocks
redisplay or your typing — which is much better, and costs exactly one
thing: *a sequence of commands is not atomic.* A keystroke can land
between two of them.

So where it matters, use the one primitive that does the whole job:

#+begin_src lisp
(defun surround-region (left right)
  (let ((r (region)))
    (if r
        (replace-region (car r) (cdr r)
                        (concatenate 'string left (region-text) right))
        (message \"no selection\"))))
#+end_src

That is from =init.lisp=, unedited. Written as =delete-region= then =insert=
it would also work, almost always, and would occasionally eat a character
somebody typed in between. =insert-at= exists for the same reason: it is
=replace-region= over an empty range, so it neither moves point nor leaves a
gap.

The exception to =writes land immediately= is the handful of things that
need the *application* rather than the editor core: =find-file=, =save-file=,
git, dired and the terminal. Those take effect a moment later, and a reader
called straight afterwards still sees the old buffer. Everything else — every
edit, every overlay, every key binding — is applied on the spot, so the next
form already sees it. That distinction is documented at the head of
=init.lisp= and is worth remembering the first time a =find-file= seems not
to have happened.

Two more that are about position rather than text:

#+begin_src lisp
(save-excursion ...)   put point back afterwards, via a marker
(with-current-buffer \"name\" ...)   work in another buffer
#+end_src

Both are macros, both expand into =unwind-protect=, and both are defined in
=crates/lisp/src/shim.c= rather than in Rust — which is the pattern this
whole editor is built on."
      :exercises
      ((:prompt
"Put the word =hello= at the very start of the scratch buffer. One call —
and not =insert=, which goes wherever point happens to be and would only
work by luck."
        :template "(insert-at (point-max) \"hello\")"
        :check (and (>= (buffer-size) 5) (string= (buffer-substring 0 5) "hello"))
        :hint "The template has the right verb and the wrong position.")
       (:prompt
"Replace the whole scratch buffer with exactly the text =(list 1 2 3)= — in
one atomic call, not a delete followed by an insert."
        :template "(delete-region (point-min) (point-max))"
        :check (string= (buffer-string) "(list 1 2 3)")
        :hint "`replace-region' takes a beginning, an end and the replacement.")
       (:prompt
"Define =tutor-shout-line=, a zero-argument command that wraps the line
point is on in =<<= and =>>= using one =replace-region= — then call it. The
check looks for a line so wrapped anywhere in the buffer."
        :template "(defun tutor-shout-line () (message \"not written yet\"))"
        :check (find-if (lambda (l)
                          (and (>= (length l) 4)
                               (string= "<<" (subseq l 0 2))
                               (string= ">>" (subseq l (- (length l) 2)))))
                        (buffer-lines))
        :hint "`(replace-region (line-start) (line-end) (concatenate 'string \"<<\" (line-string) \">>\"))', then call your function.")))

     ;; -----------------------------------------------------------------------
     (:id "commands"
      :title "Commands and keys"
      :prose
"*A command is a zero-argument function in the ZEMACS package.* That is
the whole of it. There is no =interactive= declaration, no command table to
add yourself to, and no macro to use instead of =defun=.

Key bindings and dashboard items name commands *as strings*, and anything
that is not one of the editor's built-in verbs is called in this image:

#+begin_src lisp
(define-key \"normal\" \"SPC f f\" \"find-file\")
(define-key \"normal\" \"SPC m y\" \"my-command\")
#+end_src

The first argument is a keymap: an editing state (\"normal\", \"insert\",
\"visual\", \"visual-line\", \"visual-block\", \"dashboard\", \"magit\",
\"dired\", \"terminal\") or a *mode name*, in which case the binding applies
only in that mode's buffers. Sequences are space-separated tokens: =SPC=,
=C-x=, =<esc>=, =<ret>=, =<tab>=, or a literal key.

=init.lisp= wraps this in two helpers worth copying:

#+begin_src lisp
(define-leader \"SPC o t\" \"terminal\")     the four states with a leader
(define-key-everywhere \"M-x\" \"execute-command\")
#+end_src

=define-mode-key= (from =runtime/modes/modes.lisp=) is the one to use for a
mode, because it is *inherited*: a binding made for =prog-mode= is re-issued
under every mode derived from it, which plain =define-key= does not do.

Two readers let you ask about bindings rather than remember them:

#+begin_src lisp
(key-bindings)        every binding, as (MAP KEYS COMMAND)
(where-is \"find-file\")  just that command's
#+end_src

=runtime/modes/which-key.lisp= is built out of nothing but those two, which
is why it can never go stale: it keeps no keymap of its own and asks the
editor every time.

To run a built-in verb from Lisp — one the editor resolves itself, like
=magit-status= or =split-window-below= — use =(call-command \"name\")=. It
resolves the name exactly as a key binding does.

M-x, finally: =(register-command \"name\")= offers one, =(clear-commands)=
empties the list, and =refresh-commands= in =init.lisp= does both by walking
the package for zero-argument functions."
      :exercises
      ((:prompt
"Define =tutor-shout=, which messages something in capitals, and bind it to
=SPC m y= in the \"normal\" keymap. The check looks the binding up through
the editor's own reader, so it has to have really landed."
        :template "(defun tutor-shout () (message \"quiet\"))"
        :check (and (fboundp (find-symbol "TUTOR-SHOUT" :zemacs))
                    (find-if (lambda (b) (and (string= (second b) "SPC m y")
                                              (string= (third b) "tutor-shout")))
                             (key-bindings)))
        :hint "`(define-key \"normal\" \"SPC m y\" \"tutor-shout\")' — the command is named as a string.")
       (:prompt
"Answer where =find-file= is bound, using the introspection reader rather
than reading =init.lisp=. The check insists on =SPC f f= being among the
answers *and* on every answer being about =find-file= — so handing back the
whole keymap is not the same as asking the question."
        :template "(key-bindings)"
        :check (and (listp value) (plusp (length value))
                    (every (lambda (b) (string= (third b) "find-file")) value)
                    (find "SPC f f" value :key #'second :test #'equal))
        :hint "One reader, one argument, and it filters `key-bindings' for you.")
       (:prompt
"Bind =SPC m z= to =tutor-shout= in all four states that have a leader —
normal, visual, visual-line and visual-block — with one form. The check
counts the bindings, so four separate calls also pass; a loop is the point."
        :template "(define-key \"normal\" \"SPC m z\" \"tutor-shout\")"
        :check (>= (count-if (lambda (b) (string= (second b) "SPC m z")) (key-bindings)) 4)
        :hint "`dolist' over a list of state names.")))

     ;; -----------------------------------------------------------------------
     (:id "modes"
      :title "Major modes, hooks and settings that revert"
      :prose
"A buffer has exactly one major mode and any number of minor modes. The
editor names the major mode after the language it detected — =notes.org=
opens in =org-mode= — and the *only* thing it tells this image is \"you are
now in X\", by calling the function =X-hook= if one is defined.

Everything else is Lisp, in =runtime/modes/modes.lisp=: exit hooks,
inheritance, settings that revert when you leave, choosing a mode from a
filename. That is deliberate — this is the part every config wants to bend,
and the image is where bending costs nothing.

#+begin_src lisp
(define-derived-mode NAME PARENT &body BODY)
(define-minor-mode NAME DOC (:on ...) (:off ...))
(set-mode-local MODE SETTING VALUE)
(define-mode-key MODE KEYS COMMAND)
(add-auto-mode \".rst\" 'text-mode)
(derived-mode-p 'prog-mode)      about the live buffer
(minor-mode-p 'visual-line)
#+end_src

=define-derived-mode= generates two functions: NAME, the command that puts
the live buffer in the mode and which =M-x= offers, and NAME-hook, which the
editor calls and which drives the machinery. NAME-exit-hook is yours to
write if you want one.

*Settings are claimed, not set.* The settings the editor has —
=line-overflow=, =line-numbers=, =relative-line-numbers=, =tab-width=,
=font-size= — are global, one per editor and not one per buffer. So a hook
that set =line-overflow= would leave every other mode to put it back, which
does not scale past two modes. Instead:

#+begin_src lisp
(set-mode-local 'org-mode 'line-overflow \"wrap\")
#+end_src

and the effective value is *recomputed* from the bottom up whenever the modes
change — baseline, then the major mode and its ancestors, then each active
minor mode. Recomputing rather than saving and restoring in pairs is what
makes it order-proof.

This tutor's own mode is three lines of exactly that, at the foot of
=runtime/modes/tutor.lisp=:

#+begin_src lisp
(define-derived-mode tutor-mode text-mode)
(set-mode-local 'tutor-mode 'tab-width 2)
(set-mode-local 'tutor-mode 'line-overflow \"wrap\")
#+end_src

and the reason it derives from =text-mode= rather than =lisp-mode= is
written out beside them.

One ceiling, named where it lives: switching *buffers* fires no hook, because
the editor reports none, so the settings on screen follow the last mode
*entered* rather than the buffer you are looking at."
      :exercises
      ((:prompt
"Define a major mode =tutor-demo-mode= deriving from =prog-mode=. The check
asks the mode registry whether the relationship really exists, so the
=define-derived-mode= has to have run."
        :template "(defun tutor-demo-mode () (message \"not a mode\"))"
        :check (and (fboundp (find-symbol "TUTOR-DEMO-MODE" :zemacs))
                    (derived-mode-p 'prog-mode "tutor-demo-mode"))
        :hint "`(define-derived-mode tutor-demo-mode prog-mode)' — no body needed.")
       (:prompt
"Claim =tab-width= 7 for =tutor-demo-mode=. The check reads the claim out of
=*mode-locals*=, which is the table =set-mode-local= writes into — there is
no reader for a setting's *claim*, only for its current value."
        :template "(set-tab-width 7)"
        :check (eql 7 (cdr (assoc 'tab-width (gethash "tutor-demo-mode" *mode-locals*))))
        :hint "`set-mode-local' takes the mode, the setting name as a symbol, and the value.")
       (:prompt
"Give =tutor-demo-mode= the binding =SPC m q= for =tutor-hello=, using the
inheriting form rather than plain =define-key= — so a mode derived from it
later would get the binding too. The template does it the plain way, and the
check can tell: it looks in =*mode-keys*=, the table that remembers a
binding so a child defined *later* can still inherit it, as well as in the
editor's own keymap."
        :template "(define-key \"tutor-demo-mode\" \"SPC m q\" \"tutor-hello\")"
        :check (and (assoc "SPC m q" (gethash "tutor-demo-mode" *mode-keys*)
                           :test #'string=)
                    (find-if (lambda (b) (and (string= (first b) "tutor-demo-mode")
                                              (string= (second b) "SPC m q")))
                             (key-bindings))
                    t)
        :hint "`define-mode-key' takes the mode as a symbol.")))

     ;; -----------------------------------------------------------------------
     (:id "overlays"
      :title "Overlays: everything on screen that is not text"
      :prose
"An overlay is a range of buffer text plus a property list. It moves with
the text as you type in front of it, and it dies with the text it covers.

#+begin_src lisp
(make-overlay beg end)         answers a handle, or 0
(overlay-put ov prop value)
(overlay-get ov prop)
(overlays-in beg end)          (ID START END) each, oldest first
(overlays-at pos)
(overlay-start ov) (overlay-end ov)
(delete-overlay ov)
(remove-overlays &optional beg end)
#+end_src

*Five properties are drawn*; everything else stays in this image and can
be any Lisp object at all — a closure, a diagnostic, a window id.

#+begin_example
face          a foreground, named from `face-list'
background
display       a string drawn *instead of* the characters it covers
image         an id from `latex-preview'
fold          the lines after the first stop occupying rows
#+end_example

Colours are *face names*, not RGB triples: the theme already owns that
vocabulary, so an overlay recoloured by =load-theme= costs nothing and no
second colour table exists.

=display= is the one that changes what is possible. The renderer substitutes
cells rather than painting over them, so three =***= really do become one
bullet and the heading text really does stay where it was. That mechanism is
the whole of =runtime/modes/org-modern.lisp= — and the whole of the framed
answer areas you are typing into right now, whose buffer text is the plain
comment =;;; @answer 0=.

*Mark your own.* =remove-overlays= is the blunt instrument and takes
everybody's. Put a property of your own on the ones you make and walk
=overlays-in= looking for it:

#+begin_src lisp
(remove-if-not (lambda (o) (overlay-get (first o) :latex))
               (mapcar #'first (overlays-in beg end)))
#+end_src

That is =org-latex-preview= in =init.lisp=, and it is why re-previewing
equations does not eat org-modern's bullets, or this tutor's frames, or an
avy hint somebody else's config drew.

One ceiling: an overlay the editor deletes on its own — because an edit
swallowed the text it was about — leaves its property list behind, a few
conses. =delete-overlay= prunes what it removes, which covers every
deliberate case."
      :exercises
      ((:prompt
"Make an overlay over the first line of the scratch buffer, mark it with the
property =:mine= set to T, and give it the =string= face so you can see it."
        :template "(make-overlay (line-start 1) (line-end 1))"
        :check (find-if (lambda (o) (overlay-get (first o) :mine))
                        (overlays-in (point-min) (point-max)))
        :hint "`make-overlay' answers the handle; two `overlay-put' calls on it.")
       (:prompt
"Make the first three characters of the buffer *display* as =abc=, whatever
they actually are. Look at the scratch buffer afterwards: the text is
unchanged and what you see is not."
        :template "(overlay-put (make-overlay 0 3) 'face \"keyword\")"
        :check (find-if (lambda (o) (equal (overlay-get (first o) 'display) "abc"))
                        (overlays-in 0 3))
        :hint "The property is `display' and its value is a string.")
       (:prompt
"Cleaning up after yourself. Make two overlays marked =:scrap= and one
marked =:keep=, then delete only the scrap ones — leaving the keeper alive.
=remove-overlays= would take all three, so it is not the answer."
        :template "(remove-overlays (point-min) (point-max))"
        :check (and (null (remove-if-not (lambda (o) (overlay-get (first o) :scrap))
                                         (overlays-in (point-min) (point-max))))
                    (find-if (lambda (o) (overlay-get (first o) :keep))
                             (overlays-in (point-min) (point-max))))
        :hint "Walk `overlays-in', keep the ones whose `:scrap' is true, `delete-overlay' each.")))

     ;; -----------------------------------------------------------------------
     (:id "folding"
      :title "Folding is one overlay property"
      :prose
"There is no fold object. A fold *is* an overlay carrying =fold=, so it
moves with the text, dies with it, and is undone by the same
=delete-overlay= as anything else.

#+begin_src lisp
(fold-region beg end)     hide every line after BEG's, through END's
(folds-in beg end)        the folds overlapping a range
(folded-p &optional pos)  the innermost fold covering POS, or NIL
(unfold-region beg end)   answers how many it took out
(unfold-all)
#+end_src

What is deliberately *not* here is any opinion about what is foldable. An
org subtree, a =defun=, a brace block — that is policy, and policy is
Lisp's. =runtime/modes/org-fold.lisp= is the first such policy and lives
entirely on top of these five: it decides what an org subtree is and binds
=z a=, =z M= and =z R= over it, and =*fold-subtree-functions*= is where
another mode joins in.

Note that =folds-in= is not =overlays-in=. It filters for the ones carrying
=fold=, because an org buffer is full of =display= overlays for its bullets
and folding must not eat them — the same =mark your own= discipline as the
last lesson, applied by the primitive itself.

The renderer does not draw a hidden line, and =j= steps over it, which is
the whole of folding as far as the command loop is concerned: a cursor on a
row that is not drawn is a cursor nobody can see, and the next keystroke
would edit text nobody can see.

If you have been rearranging the scratch buffer, press =C-c C-w= before
starting: these exercises talk about lines 2 to 4, and it needs to have
some."
      :exercises
      ((:prompt
"Fold lines 2 to 4 of the scratch buffer. Look at it afterwards — line 2
stays and the two below it stop taking up rows."
        :template "(make-overlay (line-start 2) (line-end 4))"
        :check (plusp (length (folds-in (line-start 2) (line-end 4))))
        :hint "One call, and it takes two character offsets rather than line numbers.")
       (:prompt
"Answer the innermost fold covering the start of line 3 — the one you just
made. The check compares it with what the editor answers, so it has to be a
real handle."
        :template "(folds-in (point-min) (point-max))"
        :check (and value (eql value (folded-p (line-start 3))))
        :hint "There is a reader whose whole job this is, and it takes an optional position.")
       (:prompt
"Open everything again — and answer how many folds you took out, which the
unfolding form already tells you. The check wants a positive number *and* an
empty buffer afterwards, so =there were none to begin with= does not count
as having opened them."
        :template "(folds-in (point-min) (point-max))"
        :check (and (integerp value) (plusp value)
                    (null (folds-in (point-min) (point-max))))
        :hint "There is a zero-argument form of `unfold-region', and it answers the count.")))

     ;; -----------------------------------------------------------------------
     (:id "prompts"
      :title "Asking the user something, without stopping the editor"
      :prose
"#+begin_src lisp
(read-string \"label: \" callback)
(completing-read \"label: \" candidates callback)
#+end_src

Both open a prompt in the minibuffer, and both answer immediately — an id,
not the text. *The answer arrives by calling your callback, later.*

This is a continuation and not a blocking read, and it has to be. The Lisp
thread evaluates one queued form at a time, so a primitive that waited for a
keystroke would stop the image — every mode hook, every key bound to a Lisp
command, every =M-x= — until you answered. That is exactly the elisp
behaviour =docs/threading.org= exists to avoid.

So the rest of your command goes *inside* the callback:

#+begin_src lisp
(defun rename-thing ()
  (read-string \"new name: \"
               (lambda (answer)
                 (when answer
                   (message (format nil \"renamed to ~a\" answer))))))
#+end_src

=answer= is NIL if the prompt was cancelled, so =(when answer ...)= is the
idiom — the same shape as =(when (region) ...)=.

Callbacks nest. Asking a second question from inside the first one's
callback is explicitly supported, and =runtime/modes/ai.lisp= does exactly
that: pick a harness, then pick new-or-resume. Two decisions, two prompts,
rather than one unreadable nine-item list.

=completing-read= takes candidates and uses the same fuzzy matcher every
other picker does. Nothing is required to match: with no hit, the answer is
what was typed, which is Emacs' =require-match= NIL and the useful default.
=tutor-goto-lesson=, which =C-c C-l= runs, is eleven lines of exactly this.

Underneath both is a table:

#+begin_src lisp
(%prompt-park closure)     park it, answer a fresh id
(%prompt-reply id answer)  call whatever is parked under ID
#+end_src

An id rather than a single variable so that a stale reply is *dropped*
rather than delivered to whoever asked most recently. The third exercise
uses those two directly, because that is the only way to prove to yourself
what a callback is without a human being in the loop.

The first two exercises are checked only for the *existence* of your
command — a prompt needs somebody to type into it, and this tutor cannot.
Run them by hand with =M-x= afterwards; that is the real check."
      :exercises
      ((:prompt
"Define =tutor-ask=, a zero-argument command that reads a string with the
label \"say something: \" and messages it back — doing nothing at all when
the prompt is cancelled. The template is a note rather than code, because
the shape it would start you from is the shape that does not work."
        :template ";; `read-string' does not answer the text. It answers an id and calls a
;; function you give it, later, with the text or with NIL. Write `tutor-ask'
;; below, and run it with M-x afterwards — that is the real check."
        :check (fboundp (find-symbol "TUTOR-ASK" :zemacs))
        :hint "`read-string' takes two arguments: the label and a function of one argument.")
       (:prompt
"Define =tutor-pick=, which offers the three candidates \"red\", \"green\"
and \"blue\" with =completing-read= and messages whichever you chose."
        :template ";; Three arguments this time, and the last one is still the callback.
;; Write `tutor-pick' below."
        :check (fboundp (find-symbol "TUTOR-PICK" :zemacs))
        :hint "Three arguments: the label, the list of candidates, and the callback.")
       (:prompt
"A callback is only a closure. Park one that stores what it is given, deliver
the string \"delivered\" to it by hand with =%prompt-reply=, and answer what
it stored — with no prompt ever opening."
        :template "(let ((got nil))
  (%prompt-park (lambda (a) (setf got a)))
  got)"
        :check (equal value "delivered")
        :hint "`%prompt-park' answers the id. Keep it, and hand it to `%prompt-reply' with the string.")))))
  "Stage 2 — the API. Checked in the live image, because the exercises are about
the live image.")

;;; ---------------------------------------------------------------------------
;;; Stage 3 — deliberately unbuilt.

(defparameter *tutor-stage-3*
  '(:id "todo"
    :title "Stage 3 — TODO"
    :check :image
    :lessons

    ((:id "todo"
      :title "What a third stage would cover"
      :prose
"This stage is a placeholder. It has no lessons and no exercises, and the
note below is the whole of it — written down rather than left implicit so
that the shape of the gap is visible to whoever fills it.

Stages 1 and 2 get you to the point of writing a plugin. A third stage would
be about the things that are *already written* in this repository, read as
worked examples, because the best way to learn a codebase's idioms is to be
walked through its own code:

  - *A language client from nothing.* =runtime/rpc.lisp= is JSON-RPC over a
    child's stdin and stdout and knows nothing about language servers;
    =runtime/lsp.lisp= is the whole client written on top of it — the
    handshake, document synchronisation, go-to-definition, diagnostics. Rust
    owns the pipe, the framing and the process, and nothing else. Registering
    a third server is one line in a config.

  - *A scanner, and the commands built on it.* =runtime/modes/lisp-mode.lisp=
    is one pass over the shape of Lisp text, and the motion, kill, slurp and
    indentation commands all read out of it. =runtime/modes/parinfer.lisp= is
    the same scanner run backwards.

  - *Rich presentation.* =runtime/modes/org-modern.lisp= at full length: the
    emphasis rules, the appear-on-cursor half, and what it costs to have no
    cursor-movement hook.

  - *Subprocesses as buffers.* =runtime/modes/ai.lisp= — three coding agents
    on the terminal the editor already has, with the harness list as *data*,
    so a fourth is one line and no Rust.

  - *Writing a test.* =crates/lisp/tests/= boots a real image against a real
    editor and waits on editor state. This is the part a plugin author needs
    and never gets taught.

  - *The boundary.* =docs/boundary.org= is the honest list of what this
    editor cannot do and why. Reading it is the last lesson of any tutorial
    about a Lisp machine: knowing where the image ends is knowing what you are
    standing on.

ponytail: not built, and the reason is scope rather than difficulty — the
machinery above is complete and a stage is a list entry. Whoever adds it
should copy Stage 2's shape exactly: =:check :image=, a =:lessons= list, and
checks that ask the editor rather than the source.

For now, =C-c C-p= goes back to Stage 2 and =C-c C-l= jumps anywhere."
      :exercises ()
      :closing
"Nothing to do here. Press =C-c C-l= to go back to a lesson, or =q= to leave
— your progress is already saved."))))

;;; ---------------------------------------------------------------------------

(setf *tutor-stages* (list *tutor-stage-1* *tutor-stage-2* *tutor-stage-3*))
