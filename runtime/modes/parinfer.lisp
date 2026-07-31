;;;; parinfer, indent mode: the indentation says where the parentheses go.
;;;;
;;;; `lisp-mode.lisp' is the other direction — the parentheses are the truth and
;;;; `lisp-indent-line' lays the text out under them. This is the inverse, which
;;;; is the whole idea of parinfer: you move a line left or right, and the
;;;; closing parentheses above it follow you.
;;;;
;;;; Built on `lisp-mode.lisp' and adding no scanner of its own. `%lisp-classes'
;;;; already answers the only hard question — a `)' inside `";)"', `#\)' or a
;;;; `#| |#' comment is punctuation and not structure — and `%line-starts',
;;;; `%line-of', `%lisp-verbatim-p' and `%toplevel-form' are all already there.
;;;; A second scanner would be a second thing to get wrong.
;;;;
;;;; The rule, in one sentence: *every closing parenthesis a line trails is
;;;; thrown away, and re-emitted where the next line's indentation puts it.* A
;;;; closing parenthesis in the middle of a line is left exactly where it is,
;;;; because something follows it and moving it would move that too.
;;;;
;;;; The important consequence is that this is a no-op on code that is already
;;;; indented the way its parentheses say — so running it costs nothing, and it
;;;; only ever *does* something after you have changed an indentation. Which is
;;;; why the two commands that shift a line are here beside the two that only
;;;; rewrite: without them there is no way to ask for the thing at all.
;;;;
;;;; * What is here: indent mode. It is the part of parinfer people actually
;;;;   use, and the only part that is small.
;;;;
;;;; * What is not: *paren mode* (you edit the parentheses and the indentation
;;;;   follows) and *smart mode* (both at once, decided from what you just
;;;;   typed). Paren mode is `lisp-indent-defun' plus a paren-trail tracker that
;;;;   has to preserve edits made inside the trail; smart mode needs to know
;;;;   *what changed*, which is precisely the delta `after-change-hook' does not
;;;;   carry (see `boundary.org'). Half-building either would give you a command
;;;;   that reformats correctly nine times and eats a form on the tenth, and the
;;;;   tenth is the one you remember.
;;;;
;;;; * Why this is not on `after-change-hook', which is what real parinfer runs
;;;;   on. Four reasons, and the last is the one that decides it:
;;;;
;;;;   1. the hook carries no delta, so every keystroke would mean rescanning
;;;;      and rewriting the whole form to find out whether an *indentation*
;;;;      changed — the one edit parinfer cares about — or whether you just
;;;;      typed a letter;
;;;;   2. the rewrite is itself a change, so it fires the hook again, and the
;;;;      guard against that is state that has to be right on every path;
;;;;   3. a rewrite is one `replace-region' over a whole top-level form, so
;;;;      undo would take back a paragraph of typing per keystroke;
;;;;   4. Lisp here is *not synchronous with the command loop* (`threading.org').
;;;;      The rewrite lands a turn of the Lisp queue after the keystroke that
;;;;      triggered it, and by then you have typed two more characters — which
;;;;      the rewrite, computed from the buffer as it was, would silently
;;;;      discard. Emacs' parinfer is safe from this because elisp freezes the
;;;;      editor while it runs. That trade is the whole reason zemacs is not
;;;;      elisp, and it is not being made back here for a paren.
;;;;
;;;;   The upgrade path, if it is ever wanted, is the change delta plus a hook
;;;;   that runs *before* the next keystroke is applied rather than after the
;;;;   last one — neither of which exists, and both of which are core's.

(in-package :zemacs)

(defparameter *parinfer-pairs* '((#\( . #\)) (#\[ . #\]))
  "Opening delimiter -> what closes it. `[' is here for the same reason
`lisp-mode.lisp' treats it as a parenthesis: a config may well be reading one,
and in Lisp the brackets are reserved for exactly this kind of extension.")

;;; ---------------------------------------------------------------------------
;;; The scan
;;;
;;; One pass down the lines, carrying a stack of the lists still open and the
;;; column each was opened at. A line's indentation is compared against that
;;; column and nothing else, which is the entire algorithm.

(defun %parinfer-trim (text classes bol end closers)
  "END walked back over trailing blanks — and, when CLOSERS, over trailing
closing parentheses as well.

Only over characters the scanner calls :CODE, which is the point of using it: a
space inside a string and a `)' inside a comment are content, and content does
not move."
  (loop while (and (> end bol)
                   (eq (aref classes (1- end)) :code)
                   (let ((c (char text (1- end))))
                     (or (%blank-char-p c)
                         (and closers (member c '(#\) #\]))))))
        do (decf end))
  end)

(defun %parinfer-target-p (classes eol n)
  "True when a closing parenthesis appended at EOL would land in code rather
than inside a string or a block comment that is still open there."
  (let ((k (if (< eol n) eol (1- n))))
    (or (minusp k) (eq (aref classes k) :code))))

(defun %parinfer (text classes starts from to)
  "What lines FROM..TO of TEXT (1-based, inclusive) become under the indent
rule, as one string with no trailing newline — or NIL when the span must not be
rewritten at all.

NIL is the important half. A closing parenthesis with nothing open means this
scan and the reader disagree about where the structure is, and moving
parentheses on a disagreement is how a structural editor eats your file. It
refuses, says so, and changes nothing.

Three kinds of line are neutral — they neither close anything nor take a
closing parenthesis of their own: a blank one, one that is only a comment, and
one that is inside a string or a `#| |#' that opened earlier. The last is
emitted verbatim, parentheses and all, because nothing in it is structure this
command is allowed to move.

The trail is *owned*, which is parinfer's bargain and worth knowing before you
press the key: a line's trailing closing parentheses are regenerated from the
indentation rather than counted, so a surplus one at the end of a line is
absorbed rather than reported. A surplus one in the *middle* of a line is the
disagreement above, and is refused."
  (let* ((n (length text))
         (lines (length starts))
         (rows nil)     ; (BODY . TAIL) conses, newest first
         (stack nil)    ; (COLUMN . CLOSER), innermost first
         (target nil))  ; the row a closing parenthesis is appended to
    (flet ((close-to (column)
             ;; Every list whose opening parenthesis sits at or right of COLUMN
             ;; is one this line has just dedented out of. Closing them means
             ;; appending to the *previous* row, which is where they were.
             (let ((out (make-string-output-stream)))
               (loop while (and stack (<= column (car (first stack))))
                     do (write-char (cdr (pop stack)) out))
               (let ((s (get-output-stream-string out)))
                 (when (and (plusp (length s)) target)
                   (setf (car target) (concatenate 'string (car target) s)))))))
      (loop for i from from to to
            do (let* ((bol (aref starts (1- i)))
                      (eol (if (< i lines) (1- (aref starts i)) n))
                      (verbatim (%lisp-verbatim-p classes bol n))
                      ;; The trailing comment, found by walking back from the
                      ;; end of the line while the class is still :COMMENT.
                      (cstart (let ((k eol))
                                (loop while (and (> k bol)
                                                 (eq (aref classes (1- k)) :comment))
                                      do (decf k))
                                k))
                      ;; ...then the last non-blank code character before it,
                      ;; ...then what survives throwing the trailing closers away.
                      (last-code (%parinfer-trim text classes bol cstart nil))
                      (body-end (if verbatim eol
                                    (%parinfer-trim text classes bol last-code t)))
                      (codep (> last-code bol))
                      (indent (- (or (position-if-not #'%blank-char-p text
                                                      :start bol :end cstart)
                                     cstart)
                                 bol)))
                 ;; 1. What this line's indentation closes, appended to the row
                 ;;    above. Before the row is built, because that is where the
                 ;;    parentheses go.
                 (when (and codep (not verbatim)) (close-to indent))
                 ;; 2. The row, as (BODY . TAIL): closing parentheses are
                 ;;    appended to BODY, so a trailing comment has to be held
                 ;;    apart from it or they would land after the `;'. The
                 ;;    whitespace in front of that comment is kept with it, so a
                 ;;    comment column survives; with no comment the same
                 ;;    whitespace is trailing and goes.
                 (let ((row (if verbatim
                                (cons (subseq text bol eol) "")
                                (cons (subseq text bol body-end)
                                      (if (< cstart eol)
                                          (subseq text last-code eol)
                                          "")))))
                   (push row rows)
                   (when (and codep (%parinfer-target-p classes eol n))
                     (setf target row)))
                 ;; 3. The parentheses this line opens and closes. The trailing
                 ;;    ones are not in the range: they are what is being moved.
                 (loop for j from bol below body-end
                       when (eq (aref classes j) :code)
                         do (let* ((c (char text j))
                                   (pair (assoc c *parinfer-pairs*)))
                              (cond (pair (push (cons (- j bol) (cdr pair)) stack))
                                    ((member c '(#\) #\]))
                                     (if stack
                                         (pop stack)
                                         (return-from %parinfer nil))))))))
      ;; Whatever is still open at the end of the span closes at the end of it.
      ;; Column 0 is at or left of every opening parenthesis, so this is "all".
      (close-to 0))
    (format nil "~{~a~^~%~}"
            (mapcar (lambda (row) (concatenate 'string (car row) (cdr row)))
                    (nreverse rows)))))

;;; ---------------------------------------------------------------------------
;;; Applying it
;;;
;;; Always exactly one `replace-region', which is the rule this editor is built
;;; on and matters more here than anywhere: the intermediate states of a
;;; parenthesis rewrite are forms with the wrong number of parentheses, and a
;;; keystroke landing in one of them is a corrupted file rather than a typo.

(defun %parinfer-run (text from to)
  "Lines FROM..TO of TEXT under the indent rule, or NIL. Clamped to TEXT."
  (let ((starts (%line-starts text)))
    (%parinfer text (%lisp-classes text) starts
               (max 1 from) (min to (length starts)))))

(defun %parinfer-replace (text a b new shift)
  "Put NEW in place of TEXT's bytes A..B, and put point back on its own line,
SHIFT columns over from where it was.

Text that did not move is not written at all, so a command that changes nothing
costs no undo step and fires no `after-change-hook'."
  (cond
    ((null new) (message "parinfer: unbalanced parentheses — nothing moved") nil)
    ((string= new (subseq text a b)) (message "parinfer: already agrees") nil)
    ;; Ends on `goto-char', which answers NIL, so a command that worked says
    ;; nothing: `eval-string' echoes the value of the last form, and a bare `T'
    ;; in the status line is noise on a key you press repeatedly.
    (t (let ((line (line-number)) (col (column)))
         (replace-region (%char-index text a) (%char-index text b) new)
         ;; The replacement covers every marker inside it, so `save-excursion'
         ;; would come back to the start of the span rather than to the cursor.
         ;; Line and column survive it, and the only thing that moved the column
         ;; is the shift this command asked for.
         (goto-char (min (point-max)
                         (+ (line-start line) (max 0 (+ col shift)))))))))

(defun %parinfer-reindent (text starts line delta)
  "TEXT with LINE's indentation moved DELTA columns, or NIL when it cannot be —
which is only ever a left shift off column 0."
  (let* ((n (length text))
         (lines (length starts))
         (bol (aref starts (1- line)))
         (eol (if (< line lines) (1- (aref starts line)) n))
         (have (- (or (position-if-not #'%blank-char-p text :start bol :end eol) eol)
                  bol))
         (want (max 0 (+ have delta))))
    (when (/= want have)
      (concatenate 'string
                   (subseq text 0 bol)
                   (make-string want :initial-element #\Space)
                   (subseq text (+ bol have))))))

(defun %parinfer-defun (delta)
  "Shift the current line's indentation DELTA columns and let the closing
parentheses follow, over the whole top-level form point is in.

The form is the right unit: its first character is at depth zero by definition,
so the scan can start with an empty stack and be sure of it — which a span
picked any other way could not be."
  (let* ((text (buffer-string))
         (classes (%lisp-classes text))
         (starts (%line-starts text))
         (lines (length starts))
         (here (%byte-index text (point)))
         (top (%toplevel-form text classes (min (length text) (1+ here)))))
    (cond
      ((null top) (message "no top-level form here"))
      (t (let* ((from (%line-of starts (car top)))
                (to (%line-of starts (max 0 (1- (cdr top)))))
                (line (line-number))
                (a (aref starts (1- from)))
                (b (if (< to lines) (1- (aref starts to)) (length text)))
                (moved (cond ((zerop delta) text)
                             ((not (<= from line to)) nil)
                             (t (%parinfer-reindent text starts line delta)))))
           (if (null moved)
               (message "parinfer: nothing to shift")
               (%parinfer-replace text a b (%parinfer-run moved from to) delta)))))))

;;; ---------------------------------------------------------------------------
;;; The commands

(defun parinfer-indent-defun ()
  "Make the closing parentheses of the top-level form under point agree with
its indentation. A no-op on a form that is already laid out."
  (%parinfer-defun 0))

(defun parinfer-indent-buffer ()
  "The same over the whole buffer, as one undo step."
  (let* ((text (buffer-string))
         (n (length text)))
    (%parinfer-replace text 0 n
                       (%parinfer-run text 1 (length (%line-starts text)))
                       0)))

(defun parinfer-shift-right ()
  "Indent this line one step and let the parentheses follow it in.

This is parinfer, and the other two commands are only its clean-up: a line that
moves right is a line that has joined the list above it, and the closing
parenthesis that used to end that list moves down past it in the same edit."
  (%parinfer-defun (tab-width)))

(defun parinfer-shift-left ()
  "Outdent this line one step and let the parentheses follow it out."
  (%parinfer-defun (- (tab-width))))

;;; ---------------------------------------------------------------------------
;;; Keys
;;;
;;; `define-mode-key' rather than `define-key', so anything derived from
;;; `lisp-mode' inherits them — the editor's mode keymap is keyed by the exact
;;; mode name and knows nothing about inheritance.
;;;
;;; The shifts get a chord rather than a leader sequence because they are the
;;; ones you press repeatedly, and `>' and `<' are already slurp and barf in
;;; this mode — which are the paren-first spelling of the same idea and worth
;;; keeping next to it.

(define-mode-key 'lisp-mode "M-]" "parinfer-shift-right")
(define-mode-key 'lisp-mode "M-[" "parinfer-shift-left")
(define-mode-key 'lisp-mode "SPC m p" "parinfer-indent-defun")
(define-mode-key 'lisp-mode "SPC m P" "parinfer-indent-buffer")
