;;;; Common Lisp editing: structure, motion, and indentation that knows what a
;;;; `let' binding list is.
;;;;
;;;; `lisp-mode' already exists — `library.lisp' derives it from `prog-mode' so
;;;; that a `.lisp' file arrives *somewhere*. This is what makes it a mode: one
;;;; reader for the shape of the text, five commands built on it, and an
;;;; indenter that follows Common Lisp convention rather than C's.
;;;;
;;;; Everything below scans `(buffer-string)' rather than asking the editor,
;;;; which is affordable exactly because it runs once per *command* and never
;;;; per keystroke — the line the boundary draws. tree-sitter is already parsing
;;;; this buffer, but its tree is not reachable from Lisp and would not answer
;;;; "which paren closes this one" any faster than counting does.
;;;;
;;;; Offsets are the awkward part, and the only part. `buffer-string' hands out
;;;; UTF-8 *bytes* in a base string while every offset the editor takes is a
;;;; *character*, so the scanners work in bytes and `%char-index' /
;;;; `%byte-index' convert at the edges. On ASCII — which Lisp source almost
;;;; always is — both are the identity, and they are the same two helpers
;;;; `search-forward' has always used.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; What each character is
;;;
;;; Counting parentheses is the whole job, and the whole difficulty is the
;;; parentheses that are not structure: `#\(', `";)"', `"a\\"b"', `#| ( |#'.
;;; One left-to-right pass classifies every character once, and every command
;;; below is then a scan that consults the classification instead of guessing.

(defparameter *lisp-whitespace* '(#\Space #\Tab #\Newline #\Return #\Page)
  "What separates one expression from the next.")

(defparameter *lisp-delimiters*
  (append *lisp-whitespace* '(#\( #\) #\[ #\] #\" #\' #\` #\, #\;))
  "What ends an atom. `[' and `]' are in here because a config may well be
reading one, and treating them as parentheses costs nothing in Lisp, where they
are reserved for exactly this kind of extension.")

(defun %lisp-classes (text)
  "One keyword per character of TEXT: :CODE, :STRING (delimiters included),
:COMMENT (`;' to end of line, and `#| |#' with nesting), or :QUOTED for the one
character a `\\' or a `#\\' protects.

Parenthesis counting looks at :CODE and nothing else, which is the entire
reason this exists — `#\\(' opens no list and `\";)\"' closes none."
  (let* ((n (length text))
         (out (make-array n :initial-element :code))
         (i 0))
    (flet ((at (k) (and (< k n) (char text k)))
           (mark (k class) (when (< k n) (setf (aref out k) class))))
      (loop while (< i n)
            do (let ((c (char text i)))
                 (cond
                   ;; `#\x' — whatever follows the backslash is data, including
                   ;; `(' and `"'. Missing this is how a paren counter loses.
                   ((and (char= c #\#) (eql (at (1+ i)) #\\))
                    (mark i :quoted) (mark (1+ i) :quoted) (mark (+ i 2) :quoted)
                    (incf i 3))
                   ;; `\x' — a single escaped character inside a symbol name.
                   ((char= c #\\)
                    (mark i :quoted) (mark (1+ i) :quoted)
                    (incf i 2))
                   ((char= c #\")
                    (mark i :string)
                    (let ((j (1+ i)) (done nil))
                      (loop while (and (< j n) (not done))
                            do (let ((d (char text j)))
                                 (mark j :string)
                                 (cond ((char= d #\\) (mark (1+ j) :string) (incf j 2))
                                       ((char= d #\") (setf done t) (incf j))
                                       (t (incf j)))))
                      (setf i j)))
                   ((char= c #\;)
                    (let ((j (or (position #\Newline text :start i) n)))
                      (loop for k from i below j do (mark k :comment))
                      (setf i j)))
                   ((and (char= c #\#) (eql (at (1+ i)) #\|))
                    (let ((depth 1) (j (+ i 2)))
                      (mark i :comment) (mark (1+ i) :comment)
                      (loop while (and (plusp depth) (< j n))
                            do (let ((d (char text j)))
                                 (mark j :comment)
                                 (cond ((and (char= d #\|) (eql (at (1+ j)) #\#))
                                        (decf depth) (mark (1+ j) :comment) (incf j 2))
                                       ((and (char= d #\#) (eql (at (1+ j)) #\|))
                                        (incf depth) (mark (1+ j) :comment) (incf j 2))
                                       (t (incf j)))))
                      (setf i j)))
                   (t (incf i))))))
    out))

(defmacro %with-lisp-scan ((text classes here) &body body)
  "Bind TEXT to the buffer, CLASSES to its syntax classes and HERE to point —
all in *byte* offsets, which is the space `buffer-string' hands out. Anything
going back to the editor converts with `%char-index'."
  `(let* ((,text (buffer-string))
          (,classes (%lisp-classes ,text))
          (,here (%byte-index ,text (point))))
     ,@body))

;;; ---------------------------------------------------------------------------
;;; Moving over expressions
;;;
;;; Every one of these takes and answers a byte index, and answers NIL rather
;;; than erroring when there is nothing there — the same contract every reader
;;; in this editor has.

(defun %blank-at-p (text classes i)
  "True when byte I is whitespace that is not inside anything, or comment."
  (let ((k (aref classes i)))
    (or (eq k :comment)
        (and (eq k :code) (member (char text i) *lisp-whitespace*)))))

(defun %skip-blanks (text classes i)
  "The first index at or after I that is neither blank nor comment."
  (let ((n (length text)))
    (loop while (and (< i n) (%blank-at-p text classes i)) do (incf i))
    i))

(defun %skip-blanks-back (text classes i)
  "The index just past the last non-blank, non-comment character before I."
  (loop while (and (plusp i) (%blank-at-p text classes (1- i))) do (decf i))
  i)

(defun %list-end (text classes i)
  "Byte index just past the `)' that closes the `(' at I, or NIL if it is never
closed."
  (let ((depth 0))
    (loop for j from i below (length text)
          do (when (eq (aref classes j) :code)
               (let ((c (char text j)))
                 (cond ((member c '(#\( #\[)) (incf depth))
                       ((member c '(#\) #\]))
                        (decf depth)
                        (when (zerop depth) (return-from %list-end (1+ j))))))))
    nil))

(defun %list-start (text classes i)
  "Byte index of the `(' that the `)' at I closes, or NIL."
  (let ((depth 0))
    (loop for j downfrom i to 0
          do (when (eq (aref classes j) :code)
               (let ((c (char text j)))
                 (cond ((member c '(#\) #\])) (incf depth))
                       ((member c '(#\( #\[))
                        (decf depth)
                        (when (zerop depth) (return-from %list-start j)))))))
    nil))

(defun %enclosing-open (text classes i)
  "Byte index of the innermost `(' still open at I, or NIL at top level."
  (let ((depth 0))
    (loop for j downfrom (1- i) to 0
          do (when (eq (aref classes j) :code)
               (let ((c (char text j)))
                 (cond ((member c '(#\) #\])) (incf depth))
                       ((member c '(#\( #\[))
                        (if (plusp depth)
                            (decf depth)
                            (return-from %enclosing-open j)))))))
    nil))

(defun %string-end (classes i)
  (let ((n (length classes)))
    (loop while (and (< i n) (eq (aref classes i) :string)) do (incf i))
    i))

(defun %string-start (classes i)
  (loop while (and (plusp i) (eq (aref classes (1- i)) :string)) do (decf i))
  i)

(defun %atom-end (text classes i)
  "Byte index just past the atom starting at I, or NIL when I is not on one."
  (let ((n (length text)) (start i))
    (loop while (and (< i n)
                     (or (eq (aref classes i) :quoted)
                         (and (eq (aref classes i) :code)
                              (not (member (char text i) *lisp-delimiters*)))))
          do (incf i))
    (and (> i start) i)))

(defun %atom-start (text classes i)
  "Byte index of the start of the atom that ends just before I."
  (loop while (and (plusp i)
                   (or (eq (aref classes (1- i)) :quoted)
                       (and (eq (aref classes (1- i)) :code)
                            (not (member (char text (1- i)) *lisp-delimiters*)))))
        do (decf i))
  i)

(defun %with-prefixes (text classes i)
  "I moved back over the reader macro characters in front of it, so that going
backwards out of `(a b)' in `'(a b)' lands on the quote — which is where Emacs
lands, and which is what makes `kill-sexp' after it take the whole thing."
  (loop while (and (plusp i)
                   (eq (aref classes (1- i)) :code)
                   (member (char text (1- i)) '(#\' #\` #\, #\@ #\#)))
        do (decf i))
  i)

(defun %sexp-end (text classes i)
  "Byte index just past the expression beginning at or after I, or NIL."
  (let* ((n (length text))
         (i (%skip-blanks text classes i)))
    (when (< i n)
      (let ((c (char text i)) (k (aref classes i)))
        (cond
          ;; A reader prefix is part of the form it prefixes, not a form.
          ((and (eq k :code) (member c '(#\' #\` #\, #\@ #\#)))
           (%sexp-end text classes (1+ i)))
          ((eq k :string) (%string-end classes i))
          ((and (eq k :code) (member c '(#\( #\[))) (%list-end text classes i))
          ((and (eq k :code) (member c '(#\) #\]))) nil)
          (t (%atom-end text classes i)))))))

(defun %sexp-start (text classes i)
  "Byte index of the start of the expression ending at or before I, or NIL."
  (let ((e (%skip-blanks-back text classes i)))
    (when (plusp e)
      (let* ((j (1- e)) (k (aref classes j)) (c (char text j))
             (start (cond ((eq k :string) (%string-start classes j))
                          ((and (eq k :code) (member c '(#\) #\])))
                           (%list-start text classes j))
                          ((and (eq k :code) (member c '(#\( #\[))) nil)
                          (t (%atom-start text classes e)))))
        (when start (%with-prefixes text classes start))))))

(defun %toplevel-form (text classes at)
  "(START . END) in bytes of the outermost form AT is inside, or — at top level
— of the last complete form before it. NIL when there is neither.

The fallback is what makes `C-c C-c' after a form do what it does in Emacs:
point sitting on the blank line under a `defun' still means that `defun'."
  (let ((open nil) (i at))
    (loop for o = (%enclosing-open text classes i)
          while o do (setf open o i o))
    (if open
        (let ((end (%list-end text classes open)))
          (and end (cons open end)))
        (let ((start (%sexp-start text classes at)))
          (and start (cons start (%skip-blanks-back text classes at)))))))

;;; ---------------------------------------------------------------------------
;;; The motion and structure commands
;;;
;;; Point sits *on* a character in Normal mode rather than between two, so
;;; "before point" has to mean "before the character point is on" — every
;;; command below reads at `(1+ here)' for the same reason the built-in
;;; `eval-last-sexp' reads at `cursor + 1'.

(defun lisp-forward-sexp ()
  "Move past the next expression."
  (%with-lisp-scan (text classes here)
    (let ((end (%sexp-end text classes here)))
      (if end
          (goto-char (%char-index text end))
          (message "no expression forward")))))

(defun lisp-backward-sexp ()
  "Move to the start of the expression before point."
  (%with-lisp-scan (text classes here)
    (let ((start (%sexp-start text classes here)))
      (if start
          (goto-char (%char-index text start))
          (message "no expression backward")))))

(defun lisp-match-paren ()
  "Jump between a parenthesis and its partner — vim's `%', spelled in sexps.
Anywhere else it jumps to the paren that encloses point, which is the question
you were about to ask next."
  (%with-lisp-scan (text classes here)
    (let* ((n (length text))
           (c (and (< here n) (char text here)))
           (code (and c (eq (aref classes here) :code)))
           (to (cond ((and code (member c '(#\( #\[)))
                      (let ((end (%list-end text classes here))) (and end (1- end))))
                     ((and code (member c '(#\) #\])))
                      (%list-start text classes here))
                     (t (%enclosing-open text classes here)))))
      (if to
          (goto-char (%char-index text to))
          (message "no matching parenthesis")))))

(defun lisp-kill-sexp ()
  "Delete the expression after point, into the register — so `p' puts it back,
and `p' somewhere else is how an expression is moved."
  (%with-lisp-scan (text classes here)
    (let ((end (%sexp-end text classes here)))
      (if end
          (let ((a (%char-index text here)) (b (%char-index text end)))
            (copy-region a b)
            (delete-region a b))
          (message "no expression to kill")))))

(defun lisp-slurp-forward ()
  "Pull the expression after this list into it — paredit's forward slurp.

`(foo bar) baz' becomes `(foo bar baz)'. One `replace-region' over the closing
paren and everything up to the end of what is being swallowed, so it is one
undo step and no keystroke can land in the middle of a list with no end."
  (%with-lisp-scan (text classes here)
    (let* ((open (%enclosing-open text classes here))
           (end (and open (%list-end text classes open)))
           (close (and end (1- end)))
           (after (and end (%sexp-end text classes end))))
      (if after
          (replace-region (%char-index text close) (%char-index text after)
                          (concatenate 'string
                                       (subseq text end after)
                                       (string (char text close))))
          (message "nothing to slurp")))))

(defun lisp-barf-forward ()
  "Push the last expression out of this list — paredit's forward barf.

`(foo bar baz)' becomes `(foo bar) baz'. Refuses on a one-element list, where
the answer would be an empty list and a stranded form."
  (%with-lisp-scan (text classes here)
    (let* ((open (%enclosing-open text classes here))
           (end (and open (%list-end text classes open)))
           (close (and end (1- end)))
           (last (and close (%sexp-start text classes close))))
      (if (and last (> last (1+ open)))
          (replace-region (%char-index text (%skip-blanks-back text classes last))
                          (%char-index text end)
                          (concatenate 'string (string (char text close)) " "
                                       (subseq text last close)))
          (message "nothing to barf")))))

;;; ---------------------------------------------------------------------------
;;; Indentation
;;;
;;; The C convention is "one step in per open brace". Lisp's is not: a form
;;; whose head takes a body indents its body by two from the *opening paren*,
;;; and a form that is a call lines its arguments up under the first one. Which
;;; of the two applies is a fact about the head symbol, so there are tables —
;;; the same shape as Emacs' `common-lisp-indent-function', and small enough to
;;; read in one go.
;;;
;;; The test that matters: running `lisp-indent-buffer' over the Lisp this
;;; editor ships leaves `which-key.lisp', `lisp-mode.lisp', `repl.lisp' and
;;; `library.lisp' untouched. An indenter that reformats its own project is not
;;; an indenter, it is an opinion.
;;;
;;; ponytail: `loop' clause *bodies* are the known gap — a `do' clause with more
;;; than one form puts the second under the `do' keyword rather than under the
;;; first form, because that needs a grammar for extended loop and nothing else
;;; here does. Write the clause as one form, which is better anyway.

(defparameter *lisp-body-forms*
  '("block" "case" "catch" "ccase" "cond" "ctypecase" "do" "do*" "dolist"
    "dotimes" "ecase" "etypecase" "eval-when" "flet" "handler-bind"
    "handler-case" "if" "labels" "lambda" "let" "let*" "locally"
    "macrolet" "multiple-value-bind" "prog" "prog1" "prog2" "progn" "restart-case"
    "return-from" "symbol-macrolet" "tagbody" "the" "typecase" "unless"
    "unwind-protect" "when")
  "Heads whose body is indented two columns from their opening paren. Anything
starting `def', `with-' or `do-' joins them by rule rather than by name, which
is what keeps `define-derived-mode' and `with-current-buffer' out of the list.

`loop' is deliberately *not* here. Its clauses line up under the first one, which
is what the default rule for a call already does — `(loop for x in xs' puts the
next line under `for', exactly as Emacs does.")

(defparameter *lisp-clause-forms* '("cond")
  "Body forms whose arguments are all *clauses* — peers of each other rather
than a body under a head. When the first clause shares the opening paren's line
the rest line up with it, and only when it does not do they fall back to two in.
Just `cond' so far: `case' and friends have a key first, which is what makes
them ordinary body forms.")

(defparameter *lisp-distinguished*
  '(("if" . 3) ("handler-case" . 1) ("handler-bind" . 1) ("restart-case" . 1)
    ("let" . 1) ("let*" . 1) ("lambda" . 1) ("multiple-value-bind" . 2)
    ("destructuring-bind" . 2) ("defun" . 2) ("defmacro" . 2) ("defmethod" . 2)
    ("dolist" . 1) ("dotimes" . 1) ("do" . 2) ("do*" . 2) ("case" . 1)
    ("typecase" . 1) ("ecase" . 1) ("etypecase" . 1) ("with-open-file" . 1)
    ("with-output-to-string" . 1) ("with-input-from-string" . 1)
    ("with-current-buffer" . 1) ("block" . 1) ("catch" . 1) ("return-from" . 1)
    ("the" . 1) ("flet" . 1) ("labels" . 1) ("macrolet" . 1) ("eval-when" . 1))
  "How many arguments a body form distinguishes from its body — Emacs' notion,
and the one thing `common-lisp-indent-function' does that a flat two-space rule
cannot. It only ever shows when a distinguished argument starts a line of its
own, which is precisely the shape everybody writes:

  (handler-case
      (risky)          ; argument 1, four in
    (error (e) ...))   ; body, two in")

(defun %lisp-body-form-p (head)
  (let* ((h (string-downcase head))
         ;; A leading `%' marks a name as internal; it says nothing about the
         ;; shape of the form. Stripped for the *prefix* rules only, so
         ;; `%with-current-buffer' indents like `with-open-file' while `%do' —
         ;; which is a call — is not mistaken for `do'.
         (bare (string-left-trim "%" h)))
    (flet ((starts (p) (and (>= (length bare) (length p))
                            (string= p bare :end2 (length p)))))
      (or (member h *lisp-body-forms* :test #'string=)
          (starts "def") (starts "with-") (starts "do-")))))

(defun %line-starts (text)
  "Byte index of the first character of every line, as a vector — so line N is
at index N-1, matching the 1-based line numbers everything else here uses. The
empty line a trailing newline produces is in it, exactly as `line-count' counts
it."
  (let ((out (list 0)))
    (loop for i from 0 below (length text)
          when (char= (char text i) #\Newline) do (push (1+ i) out))
    (coerce (nreverse out) 'vector)))

(defun %line-of (starts i)
  "The 1-based line byte I is on."
  (let ((lo 0) (hi (1- (length starts))))
    (loop while (< lo hi)
          do (let ((mid (ceiling (+ lo hi) 2)))
               (if (<= (aref starts mid) i) (setf lo mid) (setf hi (1- mid)))))
    (1+ lo)))

(defun %forms-before (text classes from bol)
  "How many complete expressions lie between FROM and BOL — which argument of
its enclosing form the line at BOL is about to be."
  (let ((n 0) (i from))
    (loop for end = (%sexp-end text classes i)
          while (and end (<= end bol))
          do (incf n) (setf i end))
    n))

(defun %lisp-indent-anchor (text classes bol)
  "Where the line starting at byte BOL takes its indentation from, as
(ANCHOR . OFFSET) meaning ANCHOR's column plus OFFSET. NIL at top level.

Four cases, and they are the whole of Lisp indentation:
  `((a 1)'    a list whose head is itself a list — one in, so a `let' binding
              list stacks under its first binding;
  `(when x'   a head that takes a body — two in from the paren, or four while
              its distinguished arguments are still being laid out;
  `(cond ((a' a clause form with its first clause already on the line — the
              clauses line up with each other rather than with the paren;
  `(foo a'    a call with an argument already on the line — under that argument."
  (let ((open (%enclosing-open text classes bol)))
    (when open
      (let* ((n (length text))
             (after (1+ open))
             (head-end (and (< after n)
                            (eq (aref classes after) :code)
                            (not (member (char text after) '(#\( #\[)))
                            (%atom-end text classes after)))
             (head (and head-end (string-downcase (subseq text after head-end))))
             ;; The first argument, when it shares the opening paren's line —
             ;; which is exactly "no newline in between", cheaper to ask than a
             ;; line number and the same question.
             (arg (and head-end
                       (let ((a (%skip-blanks text classes head-end)))
                         (and (< a n)
                              (not (find #\Newline text :start open :end a))
                              a)))))
        (cond
          ((null head) (cons open 1))
          ((and (member head *lisp-clause-forms* :test #'string=) arg) (cons arg 0))
          ((%lisp-body-form-p head)
           (let ((want (cdr (assoc head *lisp-distinguished* :test #'string=))))
             (cons open
                   (if (and want (< (%forms-before text classes head-end bol) want))
                       4
                       2))))
          (arg (cons arg 0))
          (t (cons open 1)))))))

(defun %blank-char-p (c) (member c '(#\Space #\Tab)))

(defun %lisp-verbatim-p (classes bol n)
  "True when the line starting at BOL is inside a string or inside a `#| |#'
comment that opened on an earlier line — text whose indentation is content."
  (and (< bol n)
       (let ((k (aref classes bol)))
         (or (eq k :string)
             (and (eq k :comment)
                  (plusp bol)
                  (eq (aref classes (1- bol)) :comment))))))

(defun %lisp-reindent (from to)
  "Re-indent lines FROM..TO (1-based, inclusive) and answer how many there were.

One `replace-region' for the whole span, which is the rule this editor is built
on: N separate edits would be N undo steps with N windows for a keystroke to
land in. The anchor a line indents from is always on an *earlier* line, so a
single pass can carry the shift each line has already taken and never has to
look at the text twice."
  (let* ((text (buffer-string))
         (n (length text))
         (classes (%lisp-classes text))
         (starts (%line-starts text))
         (lines (length starts))
         (from (max 1 from))
         (to (min to lines)))
    (when (<= from to)
      (let ((deltas (make-hash-table))
            (rows nil)
            (line (line-number))
            (col (column)))
        (loop for i from from to to
              do (let* ((bol (aref starts (1- i)))
                        (eol (if (< i lines) (1- (aref starts i)) n))
                        (content (subseq text bol eol))
                        (have (or (position-if-not #'%blank-char-p content)
                                  (length content)))
                        (anchor (unless (%lisp-verbatim-p classes bol n)
                                  (%lisp-indent-anchor text classes bol)))
                        (want (cond ((%lisp-verbatim-p classes bol n) have)
                                    ((null anchor) 0)
                                    (t (let* ((at (car anchor))
                                              (l (%line-of starts at)))
                                         (max 0 (+ (- at (aref starts (1- l)))
                                                   (or (gethash l deltas) 0)
                                                   (cdr anchor))))))))
                   (setf (gethash i deltas) (- want have))
                   (push (cons want (subseq content have)) rows)))
        (replace-region
         (%char-index text (aref starts (1- from)))
         (%char-index text (if (< to lines) (1- (aref starts to)) n))
         (format nil "~{~a~^~%~}"
                 (mapcar (lambda (row)
                           (concatenate 'string
                                        (make-string (car row) :initial-element #\Space)
                                        (cdr row)))
                         (nreverse rows))))
        ;; The replacement covers every marker inside it, so `save-excursion'
        ;; would come back to the start of the span rather than to the cursor.
        ;; Line and column survive it, and the column moved by exactly the
        ;; shift this line took.
        (goto-char (min (point-max)
                        (+ (line-start line)
                           (max 0 (+ col (or (gethash line deltas) 0))))))
        (1+ (- to from))))))

(defun lisp-indent-line ()
  "Re-indent the line point is on."
  (%lisp-reindent (line-number) (line-number)))

(defun lisp-indent-defun ()
  "Re-indent the top-level form point is in."
  (%with-lisp-scan (text classes here)
    (let ((top (%toplevel-form text classes (min (length text) (1+ here)))))
      (if top
          (let ((starts (%line-starts text)))
            (%lisp-reindent (%line-of starts (car top))
                            (%line-of starts (max 0 (1- (cdr top))))))
          (message "no top-level form here")))))

(defun lisp-indent-buffer ()
  "Re-indent every line of the buffer, as one undo step."
  (let ((n (%lisp-reindent 1 (line-count))))
    (message (format nil "indented ~a line~:p" (or n 0)))))

;;; ---------------------------------------------------------------------------
;;; Keys
;;;
;;; `define-mode-key' rather than `define-key', so a mode derived from
;;; `lisp-mode' inherits the lot — the editor's mode keymap is keyed by the
;;; exact mode name and knows nothing about inheritance.
;;;
;;; Mode keymaps are consulted in Normal and Visual states only; Insert goes
;;; through `insert_key', which looks at the state map alone. That is what makes
;;; `(', `)', `%', `<' and `>' safe to take here — they still type themselves
;;; while you are typing.

(define-mode-key 'lisp-mode ")" "lisp-forward-sexp")
(define-mode-key 'lisp-mode "(" "lisp-backward-sexp")
(define-mode-key 'lisp-mode "%" "lisp-match-paren")
(define-mode-key 'lisp-mode "C-M-k" "lisp-kill-sexp")
;;; Paredit spells these `C-)' and `C-}'; a `Key' here carries no shift bit on a
;;; Ctrl chord, so the bare brackets are what can actually be typed — and they
;;; read as "grow right" and "shrink right", which is what they do.
(define-mode-key 'lisp-mode ">" "lisp-slurp-forward")
(define-mode-key 'lisp-mode "<" "lisp-barf-forward")

(define-mode-key 'lisp-mode "<tab>" "lisp-indent-line")
(define-mode-key 'lisp-mode "C-M-q" "lisp-indent-defun")
(define-mode-key 'lisp-mode "SPC m =" "lisp-indent-buffer")
