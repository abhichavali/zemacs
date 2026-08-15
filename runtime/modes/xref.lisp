;;;; xref.lisp — a list of places, in a buffer you can walk.
;;;;
;;;; The hole this fills was named in `TODO.org' as "references need somewhere to
;;;; put a list", and the shape it predicted was a *generated buffer kind* in
;;;; Rust with its own keymap and a line-to-location table — the magit/dired
;;;; shape. It needed none of that, and finding out why is the interesting part.
;;;;
;;;; A generated buffer kind buys three things: the editor re-renders it, it
;;;; refuses edits, and it gets a mode. The first is what magit and dired need
;;;; and a results list does not — a list of hits is a *snapshot* of an answer,
;;;; not a view of live state, and re-rendering it would mean re-running the
;;;; search. The second is `(set-read-only t)', one primitive. The third is
;;;; `define-derived-mode', which has existed since wave 1.
;;;;
;;;; So this is an ordinary buffer, and the whole "line-to-location table" is the
;;;; line itself: rows are `PATH:LINE:TEXT', which is what ripgrep prints, what
;;;; `find-file-at' already parses, and what the grep picker has been offering
;;;; for as long as there has been one. There is no table to keep in step with
;;;; the text because the text *is* the table — delete a row and the answer has
;;;; one fewer place in it, which is exactly what `xref-replace' below is for.
;;;;
;;;; What uses it:
;;;;
;;;;   (xref-show "3 references" ROWS)   anything with a list of places
;;;;   lsp-find-references              `g r', over textDocument/references
;;;;   project-grep                     `SPC p g', ripgrep into a buffer
;;;;   xref-replace                     `r' in the listing — project-wide replace

(in-package :zemacs)

(defparameter *xref-buffer* "*xref*"
  "The one results buffer. One rather than a ring, for `create-buffer''s reason:
a second search is a new answer to the same question, and a stack of stale
answers is a thing to clean up rather than a thing to use.")

(defun %xref-location-p (line)
  "True when LINE is a `PATH:LINE:TEXT' row rather than the heading.

Two colons and a number between them, which is the same test `find-file-at' has
to pass to do anything — asked here so `RET' on the heading is a no-op instead of
a report about a file called `3 references'."
  (let* ((first-colon (position #\: line))
         (second (and first-colon (position #\: line :start (1+ first-colon)))))
    (and second
         (> second (1+ first-colon))
         (every #'digit-char-p (subseq line (1+ first-colon) second)))))

(defun %xref-path (line)
  "The path out of a `PATH:LINE:TEXT' row, or NIL."
  (and (%xref-location-p line) (subseq line 0 (position #\: line))))

(defun %xref-line-number (line)
  "The line number out of a `PATH:LINE:TEXT' row, or NIL."
  (when (%xref-location-p line)
    (let* ((a (position #\: line))
           (b (position #\: line :start (1+ a))))
      (parse-integer (subseq line (1+ a) b) :junk-allowed t))))

(define-derived-mode xref-mode nil
  "A list of places. `RET' opens one, `q' puts the list away."
  (set-buffer-read-only t))

(define-mode-key "xref-mode" "<ret>" "xref-open")
(define-mode-key "xref-mode" "q" "xref-quit")
(define-mode-key "xref-mode" "r" "xref-replace")

(defun xref-show (title rows)
  "Show ROWS — `PATH:LINE:TEXT' strings — under TITLE, and switch to them.

Read-only is lifted for the fill and put back after it, which is the whole of
why this is a function and not three calls at each call site: a buffer left
writable is a buffer somebody types into and then cannot save, and a buffer left
read-only cannot be refilled by the next search.

**TITLE goes to the status line and not into the buffer.** A heading row was the
first shape this had and it was wrong twice: it costs `RET' and the replace a
line they each have to recognise and skip, and it puts point on line 2 — where
the view keeps whatever scroll the window had, because nothing pulls a scroll
back for a document that has just become four lines long, so the heading could
sit above the top row and the listing read as though it began at the second hit.
Point on the *first* line settles that by itself: `ensure_cursor_visible' pulls
the scroll down to meet it. The rows are now the whole buffer, which is also
what makes `%xref-files' a plain walk."
  (create-buffer *xref-buffer*)
  (set-buffer-read-only nil)
  (delete-region (point-min) (point-max))
  ;; One `insert', not one per row: `docs/threading.org's rule about
  ;; all-or-nothing sequences, and the difference between one undo step and a
  ;; thousand on a big result set.
  (insert (format nil "~{~a~%~}" rows))
  (goto-char (point-min))
  (xref-mode)
  (message title)
  nil)

(defun xref-open ()
  "Open the place named on this line. Bound to `RET' in the listing."
  (let ((line (line-string)))
    (if (%xref-location-p line)
        (find-file-at line)
        (message "not a place")))
  nil)

(defun xref-quit ()
  "Put the listing away. Bound to `q'."
  (kill-buffer *xref-buffer*)
  nil)

;;; ---------------------------------------------------------------------------
;;; Project-wide replace
;;;
;;; The listing is the work list, which is the whole reason this lives here and
;;; not beside the grep picker: a picker offers you *one* of its candidates and
;;; throws the rest away, and a replace is an operation on all of them. Deleting
;;; a row before pressing `r' is how you exclude a hit — the same gesture
;;; `wgrep' has in Emacs, arrived at from the other direction, because here the
;;; rows were never anything but text.
;;;
;;; Each file is read, rewritten and written back once, whatever number of hits
;;; it has: the alternative is a buffer per file and an edit per hit, which is
;;; N undo steps in N buffers and a great deal of round-tripping through the
;;; editor for text nobody is looking at.
;;;
;;; ponytail: the replacement is literal and so is the search — the row's own
;;; text is not consulted, so a hit whose line has changed on disk since the
;;; search is replaced anyway if the pattern is still there, and skipped
;;; silently if it is not. Ceiling: a file edited between the search and the
;;; replace. The upgrade is to compare the row's TEXT against the line first and
;;; report the ones that moved.

(defun %xref-files ()
  "The files named in the listing, each once, in the order they first appear."
  (let (out)
    (dolist (line (buffer-lines) (nreverse out))
      (let ((path (%xref-path line)))
        (when (and path (not (member path out :test #'string=)))
          (push path out))))))

(defun %xref-replace-in-file (path from to)
  "Replace every FROM with TO in PATH. Answers how many, or NIL if unreadable."
  (let ((text (ignore-errors
                (with-open-file (in path :direction :input
                                         :external-format :utf-8)
                  (let ((s (make-string (file-length in))))
                    ;; `read-sequence' answers how much it actually read, which
                    ;; is *not* `file-length' the moment the file has any
                    ;; multi-byte character in it — that counts bytes and this
                    ;; counts characters.
                    (subseq s 0 (read-sequence s in)))))))
    (when text
      (let ((out (make-string-output-stream))
            (n 0)
            (at 0)
            (width (length from)))
        (loop for hit = (search from text :start2 at)
              while hit
              do (write-string text out :start at :end hit)
                 (write-string to out)
                 (incf n)
                 (setf at (+ hit width)))
        (write-string text out :start at)
        ;; `%write-file-safely' — `lsp.lisp', which loads first — rather than a
        ;; `:supersede' of somebody's source file: a numbered backup into
        ;; `~/.zemacs.d/backup/' first, then a temp file beside the target renamed
        ;; over it. A replace across a project is the write with the most to lose
        ;; and it used to be the one with the least protection.
        ;;
        ;; Zero when the write was refused, which is true rather than convenient:
        ;; the file was not changed. `%xref-replace-across' therefore counts it
        ;; untouched and says nothing further — the refusal already named the file.
        (if (and (plusp n) (not (%write-file-safely path (get-output-stream-string out))))
            0
            n)))))

(defun %xref-replace-across (files from to)
  "Do the work, and report it. The half of `xref-replace' that is not a prompt."
  (let ((hits 0) (touched 0))
    (dolist (path files)
      (let ((n (%xref-replace-in-file path from to)))
        (cond ((null n) (message (format nil "xref: cannot read ~a" path)))
              ((plusp n) (incf hits n) (incf touched)))))
    ;; Every open buffer whose file moved is now stale, and the auto-revert
    ;; sweep notices within its own interval — so this reports and lets that
    ;; happen rather than reaching into buffers it does not own.
    (message (format nil "replaced ~a in ~a file~:p" hits touched))))

(defun xref-replace ()
  "Replace a string across every file in the listing. Bound to `r'.

Two questions and then the work, which is `read-string' twice — the second
asked inside the first's callback, because a prompt is a continuation and there
is no other way to have two of them (`docs/threading.org'). The work itself is
`%xref-replace-across', so what is left here is the asking."
  (let ((files (%xref-files)))
    (if (null files)
        (message "nothing to replace in")
        (read-string
         "Replace: "
         (lambda (from)
           (when (and from (plusp (length from)))
             (read-string
              (format nil "Replace ~a with: " from)
              (lambda (to)
                (when to (%xref-replace-across files from to)))))))))
  nil)
