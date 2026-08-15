;;;; org tables — laying one out, and the three keys that walk one.
;;;;
;;;; A table in org is text and nothing else: a run of lines whose first
;;;; non-blank character is a `|'. Nothing in Rust knows that and nothing needs
;;;; to — this is rule 1 of `boundary.org' all the way down, a Lisp function
;;;; over `line-string' and `replace-region' with no new primitive, no new
;;;; reader and no new verb.
;;;;
;;;; Two halves, and the second is really the first:
;;;;
;;;;   `org-table-align'  every column padded to its widest cell, the rules
;;;;                      regenerated to match, numeric columns right-aligned
;;;;   TAB / S-TAB / RET  walk the cells, realigning as you go
;;;;
;;;; What makes a table feel like a table rather than a paragraph with bars in
;;;; it is that it is *always* laid out, and the one moment worth laying it out
;;;; in is the moment you leave a cell. So the motion keys are not a second
;;;; feature on top of the alignment; they are the only reason anyone would ever
;;;; call it.
;;;;
;;;; **Every command here is exactly one `replace-region'**, and that is not
;;;; tidiness. Lisp is not synchronous with the command loop (`threading.org'):
;;;; a delete followed by an insert has a keystroke's width between them, and a
;;;; key landing in that gap types into a table that is currently half a table.
;;;; So each command is a pure function from the rows it parsed to the rows it
;;;; wants — list surgery, no editing — and `%org-table-edit' is the single
;;;; funnel that reads the buffer, calls it, and writes the answer back once.
;;;; The text is compared before it is written, so TAB across a table that is
;;;; already aligned costs no undo step and fires no `after-change-hook'.
;;;;
;;;; Hand-parsed rather than matched, as everything in org here is: ECL ships no
;;;; regexp engine, and a table is four rules of grammar.
;;;;
;;;; Offsets are characters throughout — `line-string' answers characters and
;;;; `replace-region' takes them — so an index into a row is an index into the
;;;; buffer and there is nothing to convert.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; What a table is
;;;
;;; Three predicates, and all three are org's own rules rather than ones
;;; invented here. A row starts with `|'; a *rule* row is one with a `-'
;;; straight after that `|', which is `org-table-hline-regexp' said in Lisp; and
;;; a table is the maximal run of rows around point.

(defun %org-table-row-p (line)
  "True when LINE is a row of a table: blanks, then a `|'."
  (let ((i (position-if-not (lambda (c) (member c '(#\Space #\Tab))) line)))
    (and i (char= (char line i) #\|))))

(defun %org-table-rule-p (line)
  "True when LINE is a rule — `|---+---|' — rather than a row of cells.

A `-' immediately after the first `|', which is org's own test and is why it is
this and not \"every character is a dash or a plus\": `| - | |' passes that and
is a row with a minus sign in it."
  (let ((i (position #\| line)))
    (and i (< (1+ i) (length line)) (char= (char line (1+ i)) #\-))))

(defun %org-table-bounds ()
  "(FIRST . LAST) line numbers of the table point is on, or NIL when it is not
on one.

The predicate every command in this file opens with, and the one the keys ask
before they decide whether they are table keys at all."
  (let ((here (line-number)))
    (when (%org-table-row-p (line-string here))
      (let ((first here) (last here))
        (loop while (and (> first 1) (%org-table-row-p (line-string (1- first))))
              do (decf first))
        (loop while (and (< last (line-count))
                         (%org-table-row-p (line-string (1+ last))))
              do (incf last))
        (cons first last)))))

(defun %org-table-lines (bounds)
  "The table's lines, read once. Every reader below works off this list rather
than asking the editor again, so a command costs one pass over the table."
  (loop for l from (car bounds) to (cdr bounds) collect (line-string l)))

;;; ---------------------------------------------------------------------------
;;; Reading one
;;;
;;; A table parses to a list of rows, each either `:rule' or a list of trimmed
;;; cell strings — and every row of cells is padded to the same length here, at
;;; the parse, so that nothing below has to think about a short row. That is
;;; what makes the column commands three lines each: a column is an index that
;;; every row has.

(defun %org-table-cells (line)
  "LINE's cells, trimmed.

Everything before the first `|' is the indent and is not a cell. The `|' that
closes the row is dropped when there is one and not missed when there is not, so
a row still being typed — `| a | b' — reads as the two cells it plainly is."
  (let* ((a (position #\| line))
         (body (string-right-trim '(#\Space #\Tab) (subseq line (1+ a))))
         (n (length body))
         (body (if (and (plusp n) (char= (char body (1- n)) #\|))
                   (subseq body 0 (1- n))
                   body)))
    (mapcar (lambda (s) (string-trim '(#\Space #\Tab) s)) (split-string body #\|))))

(defun %org-table-indent (lines)
  "The whitespace the table sits in, taken from its first row.

One indent for the whole table rather than one per row: a rewrite that kept each
row's own leading blanks would preserve exactly the raggedness this command
exists to remove."
  (let ((line (first lines)))
    (subseq line 0 (position #\| line))))

(defun %org-table-parse (lines)
  "LINES as rows: `:rule', or a list of cells — every one of them the same
length, short rows padded with empty cells."
  (let* ((raw (mapcar (lambda (line)
                        (if (%org-table-rule-p line) :rule (%org-table-cells line)))
                      lines))
         (n (reduce #'max
                    (mapcar (lambda (r) (if (eq r :rule) 0 (length r))) raw)
                    :initial-value 0)))
    (mapcar (lambda (r)
              (if (eq r :rule)
                  r
                  (append r (make-list (- n (length r)) :initial-element ""))))
            raw)))

(defun %org-table-columns (rows)
  "How many columns ROWS has. Zero for a table that is nothing but rules."
  (length (find-if-not (lambda (r) (eq r :rule)) rows)))

;;; ---------------------------------------------------------------------------
;;; How wide, and which way up

(defun %org-table-widths (rows)
  "The width each column should be laid out to, in *characters*.

ponytail: characters and not cells. A CJK character occupies two columns on
screen and one here, so a table of Japanese text is padded to a ragged edge.
`char_cells' in `crates/render' is the only thing in the editor that can measure
a cell, and there is no way to ask it a question from the image — see the
`line_cells' entry in `boundary.org', which is the same ceiling from the other
side. The upgrade path is the one that entry already names: park a cell width on
the `Editor' beside `wrap_cols'.

One character minimum, so an entirely empty column still has a cell to put point
in."
  (loop for k from 0 below (%org-table-columns rows)
        collect (reduce #'max
                        (mapcar (lambda (r) (if (eq r :rule) 0 (length (nth k r))))
                                rows)
                        :initial-value 1)))

(defparameter *org-table-number-chars* "0123456789.,+-eE%$:"
  "What a number may be spelled out of.

ponytail: a character set rather than a parse. It calls `1.20', `-3', `12%' and
`10:30' numbers, which is the whole of what a column of figures holds, and it
calls `e5' one too. The cost of being wrong is a column right-aligned that you
would have left-aligned, which is a formatting opinion rather than a lost
character; a real reader is `read-from-string' with `*read-eval*' off and a
type check, the day somebody minds.")

(defun %org-table-number-p (cell)
  "True when CELL reads as a number: at least one digit, and nothing that could
not belong to one."
  (and (some #'digit-char-p cell)
       (every (lambda (c) (find c *org-table-number-chars*)) cell)))

(defun %org-table-numeric-p (rows k)
  "True when column K should be right-aligned.

org's own rule, and it is a *vote* rather than a requirement: a column is
numeric when most of its non-empty cells are numbers, which is what lets the
header — `Qty' over a column of counts — stay where it is instead of turning the
column back into prose. Empty cells abstain; a column of nothing is not numeric,
which is the answer that keeps a blank new row from flipping the layout."
  (let ((total 0) (numeric 0))
    (dolist (row rows)
      (unless (eq row :rule)
        (let ((cell (nth k row)))
          (when (plusp (length cell))
            (incf total)
            (when (%org-table-number-p cell) (incf numeric))))))
    (and (plusp total) (>= (* 2 numeric) total))))

(defun %org-table-alignments (rows)
  "T for each column that should be right-aligned, NIL for each that should not."
  (loop for k from 0 below (%org-table-columns rows)
        collect (%org-table-numeric-p rows k)))

;;; ---------------------------------------------------------------------------
;;; Writing one out
;;;
;;; `| Apple | 3   | 1.20 |' — a space either side of every cell, which is org's
;;; shape and is what makes the column of `|' land in one place. The rules are
;;; *regenerated* rather than padded: a rule row carries no information beyond
;;; where it sits, so there is nothing in it to preserve and rebuilding it is
;;; both shorter and incapable of drifting out of step with the widths.

(defun %org-table-pad (cell width right)
  "CELL padded to WIDTH, on the left when RIGHT."
  (let ((pad (make-string (max 0 (- width (length cell))) :initial-element #\Space)))
    (if right
        (concatenate 'string pad cell)
        (concatenate 'string cell pad))))

(defun %org-table-row-text (cells widths rights indent)
  "One row of cells, laid out."
  (with-output-to-string (out)
    (write-string indent out)
    (write-char #\| out)
    (loop for k from 0 below (length widths)
          do (write-char #\Space out)
             (write-string (%org-table-pad (nth k cells) (nth k widths) (nth k rights))
                           out)
             (write-string " |" out))))

(defun %org-table-rule-text (widths indent)
  "One rule row, wide enough for WIDTHS. `+' at the column boundaries, which is
what org writes and what makes a rule readable as a rule in the source."
  (with-output-to-string (out)
    (write-string indent out)
    (write-char #\| out)
    (loop for w in widths
          for k from 0
          do (when (plusp k) (write-char #\+ out))
             (write-string (make-string (+ w 2) :initial-element #\-) out))
    (write-char #\| out)))

(defun %org-table-text (rows widths rights indent)
  "The whole table as one string, with no trailing newline — the shape
`replace-region' over a line range wants."
  (format nil "~{~a~^~%~}"
          (mapcar (lambda (row)
                    (if (eq row :rule)
                        (%org-table-rule-text widths indent)
                        (%org-table-row-text row widths rights indent)))
                  rows)))

;;; ---------------------------------------------------------------------------
;;; Where point is, and where it goes

(defun %org-table-point (bounds lines)
  "(ROW . CELL) for where point is in the table, both zero-based.

The cell is the number of `|' strictly before point, less the one that opens the
row — so point anywhere in a cell, including on the bar that opens it, names
that cell."
  (let* ((row (- (line-number) (car bounds)))
         (line (nth row lines))
         (off (min (length line) (- (point) (line-start)))))
    (cons row (max 0 (1- (count #\| line :end off))))))

(defun %org-table-goal (bounds rows widths rights indent target)
  "The offset point should be at for TARGET, once the table has been written.

A row is a line, so the row index is a line number; the column is arithmetic
over the widths, since every cell before this one costs its width plus the two
spaces and the bar. A right-aligned cell's text starts at the far end of its
field, which is where you want to be typing in a column of figures."
  (let* ((row (max 0 (min (car target) (1- (length rows)))))
         (line (+ (car bounds) row))
         (cells (nth row rows)))
    (if (eq cells :rule)
        (line-start line)
        (let* ((k (max 0 (min (cdr target) (1- (length widths)))))
               (cell (nth k cells))
               (col (+ (length indent) 2
                       (loop for j from 0 below k sum (+ (nth j widths) 3))
                       (if (nth k rights) (- (nth k widths) (length cell)) 0))))
          (+ (line-start line) col)))))

;;; ---------------------------------------------------------------------------
;;; The funnel
;;;
;;; The only place in this file that touches the buffer. Every command below is
;;; a function from (ROWS, POINT) to (ROWS . TARGET) and does no editing at all,
;;; which is what makes "one `replace-region' per command" a property of the
;;; shape rather than a rule everybody has to remember.

(defun %org-table-edit (fn)
  "Read the table point is on, hand ROWS and the (ROW . CELL) point is in to FN,
and write back the (ROWS . TARGET) it answers.

The text is compared before it is written, so a command that changes nothing
costs no undo step and fires no `after-change-hook' — which matters here more
than anywhere, because TAB across an already-aligned table is the common case
and it is pressed once per cell."
  (let ((bounds (%org-table-bounds)))
    (if (null bounds)
        (message "not in a table")
        (let* ((lines (%org-table-lines bounds))
               (rows (%org-table-parse lines)))
          (if (zerop (%org-table-columns rows))
              (message "no columns")
              (let* ((answer (funcall fn rows (%org-table-point bounds lines)))
                     (rows (car answer))
                     (indent (%org-table-indent lines))
                     (widths (%org-table-widths rows))
                     (rights (%org-table-alignments rows))
                     (new (%org-table-text rows widths rights indent)))
                (unless (string= new (format nil "~{~a~^~%~}" lines))
                  (replace-region (line-start (car bounds)) (line-end (cdr bounds)) new))
                (goto-char (%org-table-goal bounds rows widths rights indent
                                            (cdr answer)))))))))

;;; ---------------------------------------------------------------------------
;;; Aligning

(defun org-table-align ()
  "Lay the table under point out: every column padded to its widest cell, the
rules regenerated to fit, and a column whose cells are mostly numbers pushed to
the right.

Point comes back to the cell it was in — the cell and not the offset, because
the cell is the thing that still exists after the row it is in has changed
width."
  (%org-table-edit (lambda (rows here) (cons rows here))))

;;; ---------------------------------------------------------------------------
;;; Walking
;;;
;;; TAB to the next cell, S-TAB back, RET down. Each of them realigns on the way,
;;; which is the whole gesture: you type into a cell, press TAB, and the table
;;; you left behind is straight.

(defun %org-table-next-row (rows row)
  "The first row of cells after ROW, or NIL. Rules are stepped over — there is
nothing in one to put point in."
  (loop for i from (1+ row) below (length rows)
        unless (eq (nth i rows) :rule) return i))

(defun %org-table-previous-row (rows row)
  "The last row of cells before ROW, or NIL."
  (loop for i from (1- row) downto 0
        unless (eq (nth i rows) :rule) return i))

(defun %org-table-forward (rows here)
  "The next cell: along the row, wrapping to the first cell of the next one, and
growing the table by an empty row when there is no next one.

Growing rather than refusing is what makes TAB the way you *write* a table
rather than only the way you move around one — you fill the last cell, press
TAB, and there is somewhere to keep typing."
  (let ((row (car here))
        (cell (cdr here))
        (n (%org-table-columns rows)))
    (if (and (not (eq (nth row rows) :rule)) (< (1+ cell) n))
        (cons rows (cons row (1+ cell)))
        (let ((next (%org-table-next-row rows row)))
          (if next
              (cons rows (cons next 0))
              (let ((grown (append rows (list (make-list n :initial-element "")))))
                (cons grown (cons (1- (length grown)) 0))))))))

(defun %org-table-backward (rows here)
  "The previous cell, wrapping to the last cell of the row above. The first cell
of the table stays where it is — there is nothing before it, and creating a row
there would be an odd thing for a key that means \"back\" to do."
  (let ((row (car here))
        (cell (cdr here))
        (n (%org-table-columns rows)))
    (cons rows
          (if (and (not (eq (nth row rows) :rule)) (plusp cell))
              (cons row (1- cell))
              (let ((prev (%org-table-previous-row rows row)))
                (if prev (cons prev (1- n)) (cons row 0)))))))

(defun %org-table-downward (rows here)
  "The cell below, in the same column, growing the table when there is none."
  (let ((row (car here))
        (cell (cdr here))
        (n (%org-table-columns rows)))
    (let ((next (%org-table-next-row rows row)))
      (if next
          (cons rows (cons next cell))
          (let ((grown (append rows (list (make-list n :initial-element "")))))
            (cons grown (cons (1- (length grown)) cell)))))))

(defun org-table-next-field ()
  "TAB inside a table: the next cell, and the table straight behind you."
  (%org-table-edit #'%org-table-forward))

(defun org-table-previous-field ()
  "S-TAB inside a table: the previous cell."
  (%org-table-edit #'%org-table-backward))

(defun org-table-next-row ()
  "RET inside a table: the cell below."
  (%org-table-edit #'%org-table-downward))

;;; ---------------------------------------------------------------------------
;;; Rows and columns
;;;
;;; All six are list surgery on the parsed rows and nothing else, because the
;;; funnel above has already made that the only thing they can be. A rule row
;;; needs no editing in any of them — it is rebuilt from the widths whatever
;;; happens to the columns.

(defun %org-table-cell-insert (cells k value)
  (append (subseq cells 0 k) (list value) (subseq cells k)))

(defun %org-table-cell-remove (cells k)
  (append (subseq cells 0 k) (subseq cells (1+ k))))

(defun %org-table-cell-swap (cells k j)
  (let ((out (copy-list cells)))
    (rotatef (nth k out) (nth j out))
    out))

(defun %org-table-map-cells (rows fn)
  "FN applied to every row of cells, rules passed through untouched."
  (mapcar (lambda (row) (if (eq row :rule) row (funcall fn row))) rows))

(defun org-table-insert-row ()
  "An empty row above this one, which is where org puts it."
  (%org-table-edit
   (lambda (rows here)
     (let ((row (car here)))
       (cons (append (subseq rows 0 row)
                     (list (make-list (%org-table-columns rows) :initial-element ""))
                     (subseq rows row))
             (cons row 0))))))

(defun org-table-delete-row ()
  "Take this row out. The last row of cells stays: a table with nothing but
rules in it has no columns and no way back."
  (%org-table-edit
   (lambda (rows here)
     (let* ((row (car here))
            (left (append (subseq rows 0 row) (subseq rows (1+ row)))))
       (if (find-if-not (lambda (r) (eq r :rule)) left)
           (cons left (cons (min row (1- (length left))) (cdr here)))
           (progn (message "the last row stays") (cons rows here)))))))

(defun org-table-insert-column ()
  "An empty column to the left of this one."
  (%org-table-edit
   (lambda (rows here)
     (let ((k (cdr here)))
       (cons (%org-table-map-cells rows (lambda (r) (%org-table-cell-insert r k "")))
             here)))))

(defun org-table-delete-column ()
  "Take this column out. The last column stays, for the reason the last row
does."
  (%org-table-edit
   (lambda (rows here)
     (let ((k (cdr here))
           (n (%org-table-columns rows)))
       (if (< n 2)
           (progn (message "the last column stays") (cons rows here))
           (cons (%org-table-map-cells rows (lambda (r) (%org-table-cell-remove r k)))
                 (cons (car here) (min k (- n 2)))))))))

(defun %org-table-move-column (delta)
  "Swap this column with the one DELTA over, and follow it."
  (%org-table-edit
   (lambda (rows here)
     (let* ((k (cdr here))
            (j (+ k delta))
            (n (%org-table-columns rows)))
       (if (or (minusp j) (>= j n))
           (progn (message "no column that way") (cons rows here))
           (cons (%org-table-map-cells rows (lambda (r) (%org-table-cell-swap r k j)))
                 (cons (car here) j)))))))

(defun org-table-move-column-left ()
  "This column and the one to its left change places."
  (%org-table-move-column -1))

(defun org-table-move-column-right ()
  "This column and the one to its right change places."
  (%org-table-move-column 1))

;;; ---------------------------------------------------------------------------
;;; Keys
;;;
;;; All three are already spoken for in an org buffer — TAB cycles a subtree,
;;; S-TAB cycles the whole outline, RET follows a link — and a key that only
;;; sometimes applies cannot simply be claimed: there is no way to decline a
;;; keystroke once a binding has swallowed it. So each of these is a dispatcher
;;; that asks `%org-table-bounds' and hands the key straight on when the answer
;;; is no. That is the same shape `org-cycle' itself already uses to fall
;;; through to `fold-dwim' off a headline, and it is why this file loads *after*
;;; the two it defers to.
;;;
;;; `define-mode-key' rather than `define-key', so `org-frozen-mode' and
;;; anything else derived from org inherits them.
;;;
;;; Normal state only, which is a real cost and is still the right answer: a
;;; mode keymap is consulted from `normal_key' and never from `insert_key', so
;;; reaching TAB while typing would mean claiming `<tab>' in the *global*
;;; `insert' map — where it is indentation, in every buffer, org or not. Emacs
;;; can do it because org-mode is a keymap and not a state. Here, you type the
;;; cell, press ESC, and TAB.
;;;
;;; ponytail: no keys for the six row and column commands. They are M-x verbs
;;; you reach for once while building a table, not keys a hand presses in a
;;; loop, and every obvious `SPC m' letter next to them is already an outline
;;; command. Bind them the day one of them turns out to be pressed twice in a
;;; row.

(defun org-table-tab ()
  "`TAB' — the next cell in a table, org's outline cycle everywhere else."
  (if (%org-table-bounds) (org-table-next-field) (org-cycle)))

(defun org-table-backtab ()
  "`S-TAB' — the previous cell in a table, the global outline cycle elsewhere."
  (if (%org-table-bounds) (org-table-previous-field) (org-global-cycle)))

(defun org-table-return ()
  "`RET' — the cell below in a table, the link under point elsewhere."
  (if (%org-table-bounds) (org-table-next-row) (org-open-at-point)))

(define-mode-key 'org-mode "<tab>" "org-table-tab")
(define-mode-key 'org-mode "<backtab>" "org-table-backtab")
(define-mode-key 'org-mode "<ret>" "org-table-return")
