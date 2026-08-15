;;;; org-structure — the keys that edit the outline rather than the text in it.
;;;;
;;;; `org-modern.lisp' decides what org *looks* like and `org-fold.lisp' decides
;;;; what a subtree *is*. This is the third of the three: the handful of keys
;;;; that make an org buffer feel like an outline you are working in rather than
;;;; a text file that happens to have stars in it — another heading, a level
;;;; deeper, a TODO, a ticked box.
;;;;
;;;; It is small on purpose. org has hundreds of commands and about eight of them
;;;; are the ones a hand reaches for without thinking, which is what "feels
;;;; native" turns out to mean in practice:
;;;;
;;;;   M-RET       another one of what this line is
;;;;   M-S-RET     ...and the same thing as a task
;;;;   M-left/right  promote / demote the heading
;;;;   M-S-left/right  promote / demote the whole subtree
;;;;   C-c C-t     cycle the TODO keyword
;;;;   C-c C-c     tick the checkbox
;;;;   TAB         cycle this subtree      (`org-fold.lisp')
;;;;   S-TAB       cycle the whole buffer
;;;;
;;;; Hand-parsed rather than matched throughout: ECL ships no regexp engine,
;;;; which is the same reason `lsp.lisp' splits strings by hand. An outline is
;;;; little enough grammar that this is shorter than the regexps would have been.
;;;;
;;;; Everything here reads `line-string' and hands offsets to `replace-region',
;;;; and both are counted in characters — so an index into a line is an index
;;;; into the buffer and there is nothing to convert. It was not always: the
;;;; reader answered UTF-8 bytes, and every index below was written to stop at
;;;; the last ASCII character of a line's prefix so that the two units could not
;;;; disagree. That rule is no longer load-bearing, and the code still keeps it
;;;; because moving a heading's text around whole is the right shape anyway.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; Staying where you were

(defmacro %org-keeping-point (&body body)
  "Run BODY and put point back on the same line, at the same column.

**Not a nicety.** `replace-region' leaves point at the start of whatever it
replaced — see `rs_replace_region', which sets the cursor rather than moving it
— so every command below would otherwise drag the cursor somewhere. Ticking a
checkbox was the bad one: recounting an ancestor's statistics cookie is a
`replace-region' on *that* heading's line, so `C-c C-c' left point up on the
parent heading and the next key you pressed edited the wrong thing.

A line and a column rather than `save-excursion', which is the usual answer and
is the wrong one here. `save-excursion' restores a *marker*, and the commands
below rewrite whole lines: a marker inside a replaced range collapses to the
start of it, so point would come back in column 0 of the line it started on —
better than another line, still not where you were.

Sound because none of these edits adds or removes a line, so the line number
point started on still names the same line afterwards. A command that inserts
one — `org-meta-return' — does not use this and moves point on purpose."
  (let ((line (gensym)) (col (gensym)))
    `(let ((,line (line-number))
           (,col (- (point) (line-start))))
       ,@body
       (goto-char (min (+ (line-start ,line) ,col) (line-end ,line))))))

;;; ---------------------------------------------------------------------------
;;; What kind of line is this
;;;
;;; Three readers, each answering the *prefix a new line of the same kind wants*
;;; rather than a boolean. That shape is why `org-meta-return' below is a single
;;; `or': "another one of these" is the same operation for all three, and only
;;; the string differs.

(defun %org-list-prefix (line)
  "The bullet a new item under LINE should start with, or NIL if LINE is not one.

An ordered item counts on — `3.' is followed by `4.' — and nothing renumbers the
items *below* the new one. That is org's own behaviour and not a shortcut: org
renumbers on demand, and doing it here would rewrite lines you cannot see every
time you pressed the key."
  (let* ((n (length line))
         (i (or (position-if-not (lambda (c) (member c '(#\Space #\Tab))) line) n))
         (indent (subseq line 0 i))
         (after (lambda (j) (and (< (1+ j) n) (char= (char line (1+ j)) #\Space)))))
    (cond
      ;; `-' and `+' anywhere; `*' only when indented, since a `*' in column 0
      ;; is a heading and org reads it that way too.
      ((and (< i n) (member (char line i) '(#\- #\+)) (funcall after i))
       (format nil "~a~a " indent (char line i)))
      ((and (< i n) (plusp i) (char= (char line i) #\*) (funcall after i))
       (format nil "~a* " indent))
      ;; `12.' or `12)', then a space.
      (t (let ((j (position-if-not #'digit-char-p line :start i)))
           (when (and j (> j i)
                      (member (char line j) '(#\. #\)))
                      (funcall after j))
             (format nil "~a~d~a " indent
                     (1+ (parse-integer (subseq line i j)))
                     (char line j))))))))

(defun %org-checkbox-index (line)
  "Index of the `[' of LINE's checkbox, or NIL when the item has none.

`%org-checkbox-at' from `org-modern.lisp' is the parser — one rule for what a
cookie is, shared with the thing that draws it — and this is the part it cannot
do: finding where the bullet ends, so there is a place to look from. A cookie
anywhere else on the line is prose (`the [x] column'), which is why this starts
from the bullet rather than searching."
  (let* ((n (length line))
         (i (or (position-if-not (lambda (c) (member c '(#\Space #\Tab))) line) n))
         (bullet (position #\Space line :start (min i n))))
    (when (and bullet (%org-list-prefix line))
      (let ((box (%org-checkbox-at line (1+ bullet))))
        (car box)))))

(defun %org-checkbox-prefix (line)
  "The `- [ ] ' a new item under LINE should start with, or NIL for no checkbox.

Always an *empty* box, whatever this item's is: the new item is a thing still to
do, and org agrees — `M-RET' under a ticked item gives you an unticked one."
  (let ((bullet (%org-list-prefix line)))
    (when (and bullet (%org-checkbox-index line))
      (concatenate 'string bullet "[ ] "))))

(defun %org-box-index (line)
  "Index of the `[' of LINE's checkbox, whether LINE is a headline or an item.

The two live in different places — an item's box follows its bullet, a
headline's follows its stars and its TODO keyword — and every caller below wants
the same thing from both, so the split is here and nowhere else."
  (let ((stars (%org-line-level line)))
    (if stars
        (car (%org-heading-box line stars))
        (%org-checkbox-index line))))

(defun %org-box-ticked-p (line k)
  "True when the checkbox at index K of LINE is anything but empty.

`[-]' — org's partial state — counts as ticked here, which is the answer that
makes `C-c C-c' on one clear it rather than cycling into a third state nobody
asked for."
  (not (char= (char line (1+ k)) #\Space)))

(defun %org-heading-prefix (line)
  "The stars a sibling of headline LINE should start with, or NIL.

`%org-line-level' applies org's own rule — stars at column 0 *followed by a
space* — which is what keeps `**bold**' at the start of a line from being read
as a level-2 heading.

Carries an empty checkbox when this heading has one, for the reason the list
reader does: `M-RET' under a task gives you another task. The *keyword* is not
carried — a fresh heading has no state, which is Emacs' split between `M-RET'
and `M-S-RET' and is why the new box is empty rather than copied."
  (let ((n (%org-line-level line)))
    (when n
      (format nil "~a ~a" (make-string n :initial-element #\*)
              (if (%org-heading-box line n) "[ ] " "")))))

(defun %org-new-heading-prefix ()
  "The stars a heading made *here* should start with.

The level of the headline point is living under, so `M-RET' in the middle of a
paragraph gives you a sibling of the section you are writing in rather than a
top-level heading dropped into the middle of it. `* ' in the preamble, where
there is no section to be a sibling of."
  (let ((above (%org-headline-above)))
    (format nil "~a "
            (make-string (if above (%org-level above) 1) :initial-element #\*))))

;;; ---------------------------------------------------------------------------
;;; M-RET

(defun %org-open-line-below (prefix)
  "Start a new line below this one beginning with PREFIX, and type into it.

`insert-at' and not `goto-char' then `insert', and this is the bug the whole
command used to have: in Normal state the cursor cannot sit past the last
character of a line — `clamp_cursor' puts it back — so `(goto-char (line-end))'
landed one character short and the insert *split the line it was continuing*.
`- one' with point anywhere on it became `- on' and `- e'. `insert-at' is a
`replace-region' of an empty range, so it goes where it is told and moves point
for nobody.

Insert state is entered *before* the `goto-char' for the same reason, and the
order matters: the clamp is lifted in Insert, so this is the one arrangement in
which point can be put at the end of the new line rather than on the last
character of it."
  (let ((at (line-end)))
    (insert-at at (format nil "~%~a" prefix))
    (set-evil-state "insert")
    (goto-char (+ at 1 (length prefix)))))

(defun org-meta-return ()
  "`M-RET' — another one of whatever this line is.

On a headline, a sibling headline at the same level. On a list item, another
item, carrying the bullet and counting an ordered list on — and carrying an
empty checkbox when this item has one. Anywhere else, a new headline at the
level of the section point is in, which is what org does with `M-RET' in the
middle of a paragraph and is a great deal more useful than refusing.

The four cases are one `or' because they are one operation: the readers above
each answer a *prefix*, so \"another one of these\" differs only in the string.

Order matters in exactly one place — the checkbox reader is asked before the
plain list reader, since every checkbox item is also a list item and the plain
answer would win and drop the box.

A new *task* recounts the cookies above it, because a `[2/3]' that is still
`[2/3]' after you added a fourth thing to do is worse than no cookie at all.
Only a task: the test is whether the prefix carries a box, so the ordinary
`M-RET' down a prose outline does not pay for a buffer scan per press."
  (when (derived-mode-p "org-mode")
    (let* ((line (line-string))
           (prefix (or (%org-heading-prefix line)
                       (%org-checkbox-prefix line)
                       (%org-list-prefix line)
                       (%org-new-heading-prefix))))
      (%org-open-line-below prefix)
      ;; After the insert, so the new box is counted — and *not* wrapped in
      ;; `%org-keeping-point', which would undo the move into the new line that
      ;; is the whole point of the key. The cookie pass keeps point itself.
      (when (find #\[ prefix) (org-update-statistics-cookies)))))

;;; ---------------------------------------------------------------------------
;;; M-S-RET

(defun %org-todo-heading-prefix (line)
  "The stars, keyword and box a *task* started under LINE wants.

`%org-heading-prefix''s answer with a keyword put onto it — put on by
`%org-heading-with', which is the single writer of a heading's keyword and its
box. Spelling the string out here instead would make a second writer of both,
and two writers disagreeing is the exact bug that function exists to prevent.

The trim is because it writes a *heading* rather than a prefix: it drops the
trailing space when the keyword is the whole line and keeps the box's when it is
not, so its two shapes are normalised to one here rather than at the call."
  (let* ((sibling (or (%org-heading-prefix line) (%org-new-heading-prefix)))
         (stars (%org-line-level sibling)))
    (format nil "~a "
            (string-right-trim " " (%org-heading-with sibling stars nil t)))))

(defun org-insert-todo-heading ()
  "`M-S-RET' — another heading, already carrying a TODO keyword.

`M-RET''s twin, and Emacs' own split between the two: a plain sibling starts
with no state at all, and this is the one that starts life as a thing to do. The
box comes across from `%org-heading-prefix' — empty, since a new task is not
done — and the keyword is the not-done one `%org-heading-with' writes, which is
`TODO', the first of `*org-todo-keywords*'.

Off a heading this makes one at the level of the section point is in, exactly as
`M-RET' does. `M-S-RET' in the middle of a paragraph means \"I have just thought
of something to do\", and refusing there would be the answer nobody wants."
  (when (derived-mode-p "org-mode")
    (let ((prefix (%org-todo-heading-prefix (line-string))))
      (%org-open-line-below prefix)
      ;; A new box is a box an ancestor's cookie has not counted —
      ;; `org-meta-return' above carries the reasoning.
      (when (find #\[ prefix) (org-update-statistics-cookies)))))

;;; ---------------------------------------------------------------------------
;;; Promote and demote
;;;
;;; Both halves of Emacs' split, at last: `M-left'/`M-right' move the headline
;;; alone, `M-S-left'/`M-S-right' move it with every descendant. The second pair
;;; is the one a hand reaches for — demoting a section out from under its own
;;; children is almost never what anyone means — and it could not be written down
;;; at all until `Key' learned to spell shift.

(defun %org-reheading (delta)
  "Add DELTA stars to the headline point is on. Level 1 is the floor.

One `replace-region' over the stars alone, so the heading's text, its TODO
keyword and anything else on the line are not rewritten — an edit that replaces
a line with a string it built is an edit that loses whatever it forgot to
carry."
  (let* ((line (line-string))
         (n (%org-line-level line)))
    (cond
      ((null n) (message "not on a headline"))
      (t (let ((want (max 1 (+ n delta))))
           (unless (= want n)
             (%org-keeping-point
              (replace-region (line-start) (+ (line-start) n)
                              (make-string want :initial-element #\*))))
           (message (format nil "level ~d" want)))))))

(defun org-do-demote () "One level deeper." (%org-reheading 1))
(defun org-do-promote () "One level shallower, floored at 1." (%org-reheading -1))

(defun %org-subtree-headlines (line)
  "LINE and every headline beneath it, as line numbers.

`%org-subtree-end''s rule — a subtree runs until a headline of the same or
shallower level — asked about the headlines rather than about the text, because
those are the only lines a promote has anything to do to."
  (let ((level (%org-level line)))
    (loop for l from line to (line-count)
          for lv = (%org-level l)
          while (or (= l line) (null lv) (> lv level))
          when lv collect l)))

(defun %org-reheading-subtree (delta)
  "Add DELTA stars to the headline point is on and to every headline under it.

The *same* DELTA to every one of them, clamped once against the root: level 1 is
the floor, and letting a descendant move further than its root would flatten the
shape this command exists to carry.

Applied **bottom-up**, which is what makes several edits in one pass safe: a
heading whose stars changed is a line whose length changed, so every offset below
it has moved and none above it has. `org-update-statistics-cookies' orders its
edits the same way for the same reason.

`%org-keeping-point' holds here because its precondition holds: only the run of
stars at the front of a line changes, so no line is added or removed and the
number point started on still names the line it started on."
  (let ((line (%org-headline-above)))
    (if (null line)
        (message "not on a headline")
        (let* ((level (%org-level line))
               (delta (max delta (- 1 level))))
          (if (zerop delta)
              (message "level 1")
              (progn
                (%org-keeping-point
                 (dolist (l (reverse (%org-subtree-headlines line)))
                   (let ((start (line-start l))
                         (n (%org-level l)))
                     (replace-region start (+ start n)
                                     (make-string (+ n delta)
                                                  :initial-element #\*)))))
                (message (format nil "level ~d" (+ level delta)))))))))

(defun org-demote-subtree ()
  "The heading and everything under it, one level deeper."
  (%org-reheading-subtree 1))
(defun org-promote-subtree ()
  "The heading and everything under it, one level shallower."
  (%org-reheading-subtree -1))

;;; ---------------------------------------------------------------------------
;;; TODO

;;; `*org-todo-keywords*' lives in `org-modern.lisp' with the rest of org's
;;; vocabulary, because the file that *draws* a heading needs it too: a
;;; headline's checkbox sits after its keyword, so finding the box means knowing
;;; where the keyword ends. That file loads first and depends on nothing, which
;;; is the direction the dependency has to point.

(defun org-todo ()
  "Cycle this headline's TODO keyword: none -> TODO -> DONE -> none.

A checkbox on the heading follows the keyword — cycling to DONE ticks it, and
cycling anywhere else clears it — because the two are one state wearing two
names. `%org-heading-with' is the single writer that guarantees it.

The keyword is the first word after the stars and counts only when it is one of
`*org-todo-keywords*', so a heading that begins with the word `Doneness' is
prose and is left alone. That is org's rule too."
  (let* ((line (line-string))
         (stars (%org-line-level line)))
    (if (null stars)
        (message "not on a headline")
        (let* ((now (%org-heading-keyword line stars))
               (at (member now *org-todo-keywords* :test #'equal))
               ;; The three states of the cycle, said in this file's two
               ;; variables: none is "no keyword, not done", and the last
               ;; keyword in the list is the one that means done.
               (next (if at (second at) (first *org-todo-keywords*)))
               (done (equal next (car (last *org-todo-keywords*)))))
          (%org-keeping-point
           (replace-region (line-start) (line-end)
                           (%org-heading-with line stars done (and next t)))
           (org-update-statistics-cookies))
          (message (or next "no keyword"))))))

;;; ---------------------------------------------------------------------------
;;; Statistics cookies
;;;
;;; `* Tasks [2/3]' — a heading counting the checkboxes underneath it. org keeps
;;; these current as you tick things, and a cookie that does not is worse than
;;; no cookie at all: it is a number on the screen that is wrong.
;;;
;;; ponytail: every box in the subtree counts, not just the direct children org
;;; would count. Right for the shape these are actually used in — a heading with
;;; a checklist under it — and wrong only for a cookie whose subtree holds
;;; *another* cookie, where the inner boxes are counted twice. The upgrade is to
;;; stop descending at a heading that carries its own cookie, which is three
;;; lines in `%org-count-boxes' the day somebody nests two.

(defun %org-cookie-body-p (body)
  "True for `n/m', `/', `n%' or `%' — the insides of a statistics cookie.

The empty forms are org's own: you write `[/]' and org fills it in. Accepting
them is what lets a cookie be *typed* rather than only updated."
  (let ((n (length body))
        (slash (position #\/ body)))
    (cond (slash (and (every #'digit-char-p (subseq body 0 slash))
                      (every #'digit-char-p (subseq body (1+ slash)))))
          ((and (plusp n) (char= (char body (1- n)) #\%))
           (every #'digit-char-p (subseq body 0 (1- n))))
          (t nil))))

(defun %org-cookie-index (line)
  "(BEG . END) of LINE's first statistics cookie, or NIL.

Searched across the whole line, unlike a checkbox: a cookie reads as a cookie
wherever it sits, and `* Tasks [2/3]' puts it at the end where org does. Nothing
else in org's syntax spells `[n/m]', so there is no prose to collide with."
  (loop for i = (position #\[ line) then (position #\[ line :start (1+ i))
        while i
        do (let ((j (position #\] line :start i)))
             (when (and j (%org-cookie-body-p (subseq line (1+ i) j)))
               (return (cons i (1+ j)))))))

(defun %org-cookie-text (line cookie ticked total)
  "The cookie LINE should carry, keeping the form it was written in.

`[2/3]' stays a fraction and `[67%]' stays a percentage, because which one is
there is the author's choice and rewriting it to the other would be this
command editing prose it was not asked about. A percentage of nothing is 100 —
org's answer, and the one that reads right for a heading with no boxes yet."
  (if (find #\% (subseq line (car cookie) (cdr cookie)))
      (format nil "[~d%]" (if (zerop total) 100 (round (* 100 ticked) total)))
      (format nil "[~d/~d]" ticked total)))

(defun %org-subtree-boxes (lines i level)
  "(TICKED . TOTAL) over the checkboxes under LINES[I], a headline of LEVEL.

The subtree runs until a headline of the same or shallower level, which is
`%org-subtree-end''s rule in `org-fold.lisp' — the same definition, applied to
text already read rather than by asking the editor for a line at a time.

A **vector** and an index, not a list and a line number: this is called once per
cookie and walks a subtree each time, and `nth' down a list would have made the
pair of loops quadratic on a file with a cookie near the top."
  (let ((ticked 0) (total 0))
    (loop for j from (1+ i) below (length lines)
          do (let* ((line (aref lines j))
                    (lv (%org-line-level line)))
               (when (and lv (<= lv level)) (return))
               (let ((k (%org-box-index line)))
                 (when k
                   (incf total)
                   (when (%org-box-ticked-p line k) (incf ticked))))))
    (cons ticked total)))

(defun org-update-statistics-cookies ()
  "Recount every statistics cookie in the buffer. Answers how many changed.

The whole buffer rather than the ancestors of point, and that is a deliberate
trade: walking ancestors is cheaper, but a cookie is wrong *silently*, and the
one thing worse than recounting a heading nobody touched is showing `[1/3]' over
three ticked boxes because the edit that fixed it happened somewhere this
command declined to look. One `buffer-string', and the rest is arithmetic.

Edits are applied **bottom-up**, which is what makes several of them safe in one
pass: rewriting a cookie changes the length of its line, so every character
offset below it moves and none above it does. The forward walk pushes onto a
list, so the list comes out in the order this needs."
  (let* ((lines (coerce (mapcar #'first (%org-lines)) 'vector))
         (edits nil))
    (dotimes (i (length lines))
      (let* ((line (aref lines i))
             (level (%org-line-level line))
             (cookie (and level (%org-cookie-index line))))
        (when cookie
          (let* ((count (%org-subtree-boxes lines i level))
                 (want (%org-cookie-text line cookie (car count) (cdr count))))
            (unless (string= want (subseq line (car cookie) (cdr cookie)))
              (push (list (1+ i) cookie want) edits))))))
    (%org-keeping-point
      (dolist (e edits)
        (destructuring-bind (n cookie want) e
          (let ((start (line-start n)))
            (replace-region (+ start (car cookie)) (+ start (cdr cookie)) want)))))
    (length edits)))

;;; ---------------------------------------------------------------------------
;;; Checkboxes
;;;
;;; A headline's box and its TODO keyword are **two names for one state**. `* TODO
;;; [ ] ship' ticked is `* DONE [X] ship', and cycling the keyword to DONE ticks
;;; the box — so the two can never sit on screen disagreeing with each other,
;;; which is the failure a heading carrying both would otherwise invite.
;;;
;;; Both directions go through `%org-set-heading-state' and it rewrites the whole
;;; line in **one** `replace-region'. Two edits — one for the box, one for the
;;; keyword — would be two undo steps and, worse, two windows for a keystroke to
;;; land between them (see the note on `replace-region' in `init.lisp'), which is
;;; how a heading ends up half-toggled.

(defun %org-heading-with (line stars done keyword-p)
  "Headline LINE rewritten into state DONE, keeping a TODO keyword if KEYWORD-P.

**This is the tie, and its shape is the point.** The state is *one* argument —
DONE — and both names for it are written from that one argument: the box is
ticked iff DONE, and the keyword, when the heading carries one at all, is
`DONE' iff DONE. So there is no way to call this that leaves the two
disagreeing, and no other function in this file writes either of them.

KEYWORD-P is separate because it is a different question: not *what* state the
heading is in but whether it says so in words. `* [ ] ship' and
`* TODO [ ] ship' are the same state; the second one is wordier. It is what
lets `C-c C-t' walk off the end of the cycle back to a bare heading while
`C-c C-c' leaves the wording exactly as the author had it.

Answers a whole line, and carries everything it was not asked about across
untouched: the stars, the text, a statistics cookie, a tag. Built
from its parts instead, this would silently drop whatever it forgot to think of.

No trailing space when the keyword is the whole heading — `** TODO' cycled is
`** DONE', not `** DONE ', and that space would survive every later press."
  (let* ((head (%org-heading-head line stars))
         (body (%org-heading-body line stars))
         (box (%org-heading-box line stars))
         (rest (subseq line body))
         (keyword (and keyword-p (if done "DONE" "TODO"))))
    ;; The box is ticked inside REST, where its index is relative to BODY — so
    ;; swapping a keyword of a different length cannot move it out from under us.
    (when box
      (let ((k (- (car box) body)))
        (setf rest (concatenate 'string
                                (subseq rest 0 (1+ k))
                                (if done "X" " ")
                                (subseq rest (+ k 2))))))
    (concatenate 'string
                 (subseq line 0 head)
                 (cond ((null keyword) "")
                       ((zerop (length rest)) keyword)
                       (t (format nil "~a " keyword)))
                 rest)))

(defun org-toggle-checkbox ()
  "Tick or untick the checkbox on this line. Answers T when there was one.

On a headline the TODO keyword moves with it — see the note above — and on a
list item it is just the box. Either way the cookies are recounted afterwards,
which is the whole reason a cookie is worth writing down.

Answering rather than reporting, because `org-ctrl-c-ctrl-c' below is the caller
that has something to do when the answer is no."
  (let* ((line (line-string))
         (k (%org-box-index line)))
    (when k
      (let ((done (not (%org-box-ticked-p line k)))
            (stars (%org-line-level line)))
        (%org-keeping-point
         (if stars
             ;; One `replace-region' over the whole line, not one for the box and
             ;; another for the keyword: two edits are two undo steps and two
             ;; windows for a keystroke to land in the middle of a toggle.
             ;;
             ;; The heading keeps whatever wording it had — ticking a box does
             ;; not put a keyword on a heading that never wanted one, and does
             ;; not take one off a heading that did.
             (replace-region (line-start) (line-end)
                             (%org-heading-with
                              line stars done
                              (and (%org-heading-keyword line stars) t)))
             (replace-region (+ (line-start) k 1) (+ (line-start) k 2)
                             (if done "X" " ")))
         (org-update-statistics-cookies))
        t))))

(defun org-ctrl-c-ctrl-c ()
  "`C-c C-c' — org's do-what-I-mean key.

A table under point is aligned, which is what `C-c C-c' means on one in org and
is why it comes first: a table is the one thing this key lands on where the
other three answers would all be wrong. A checkbox is ticked, and a headline's
keyword goes with it. Failing that the cookies are recounted, which is what
`C-c C-c' on a heading means in org and is the useful answer on a `* Tasks
[2/3]' that has no box of its own. Failing *that* the buffer's markup is redrawn
— \"take another look at this\", which is the right answer for a real reason
rather than as a consolation: `org-modern-refresh-line' redraws around where you
are typing, and text that arrived some other way — a program writing into the
buffer, a `p' in a pane you are not standing in — is what it cannot have seen.

`fboundp' on the table half, because `org-table.lisp' is a module a config may
have dropped from `*runtime-modules*' and this file loads before it either way."
  (cond ((and (fboundp 'org-table-align) (%org-table-bounds)) (org-table-align))
        ((org-toggle-checkbox))
        ((plusp (org-update-statistics-cookies)) (message "cookies updated"))
        (t (org-modern-refresh))))

;;; ---------------------------------------------------------------------------
;;; S-TAB
;;;
;;; TAB cycles the subtree under point and lives in `org-fold.lisp' with the
;;; three-state machine it drives. This is its global twin, and it is two-state
;;; rather than three: `fold-all' and `fold-open-all' already exist and are
;;; org's OVERVIEW and SHOWALL, while CONTENTS wants a fold per heading *level*
;;; and is the state nobody stops on.

(defun org-global-cycle ()
  "`S-TAB' — fold every subtree in the buffer, or open them all again."
  (if (folds-in (point-min) (point-max))
      (fold-open-all)
      (fold-all)))

;;; ---------------------------------------------------------------------------
;;; Keys
;;;
;;; `M-RET' is bound twice and it has to be. `normal_key' consults a buffer's
;;; *major mode* keymap; `insert_key' consults only the `insert' one — so a
;;; binding made for `org-mode' alone would be dead while you were typing the
;;; list, which is the whole time you want it. The command answers NIL outside an
;;; org buffer, so the `insert' binding is a dead key everywhere else.
(define-key "org-mode" "M-<ret>" "org-meta-return")
(define-key "insert" "M-<ret>" "org-meta-return")
;;; ...and its twin, twice for the same reason: a task is a thing you think of
;;; while typing the list, which is exactly when the mode keymap is not consulted.
(define-key "org-mode" "M-S-<ret>" "org-insert-todo-heading")
(define-key "insert" "M-S-<ret>" "org-insert-todo-heading")

;;; The rest are Normal state only, and that is a decision rather than an
;;; oversight. The same asymmetry above says why it cannot be otherwise: a mode
;;; keymap is not consulted from Insert, so reaching these while typing would
;;; mean claiming `M-<left>' in the *global* `insert' map — where it is
;;; word-at-a-time motion, in every buffer, org or not. Restructuring an outline
;;; is a Normal-state gesture in an editor with modes, which is what this is.
(define-key "org-mode" "M-<left>" "org-do-promote")
(define-key "org-mode" "M-<right>" "org-do-demote")
(define-key "org-mode" "M-S-<left>" "org-promote-subtree")
(define-key "org-mode" "M-S-<right>" "org-demote-subtree")
(define-key "org-mode" "<backtab>" "org-global-cycle")

;;; `C-c' is bound globally to `eval-dwim' and is a *prefix* here: a mode-local
;;; prefix outranks a global exact binding — see `normal_key' — so these are
;;; reachable in org buffers and `C-c' still evaluates everywhere else.
(define-key "org-mode" "C-c C-t" "org-todo")
(define-key "org-mode" "C-c C-c" "org-ctrl-c-ctrl-c")

;;; ...and under the leader, which is where which-key can tell you they exist.
;;; `h'/`l' for promote/demote rather than arrows: they are vim's own left and
;;; right, and this is the buffer you would be reaching for them in.
(define-key "org-mode" "SPC m t" "org-todo")
(define-key "org-mode" "SPC m x" "org-toggle-checkbox")
(define-key "org-mode" "SPC m h" "org-do-promote")
(define-key "org-mode" "SPC m l" "org-do-demote")
(define-key "org-mode" "SPC m <ret>" "org-meta-return")
;;; Capitals for the wider version of the same verb, which is the shift key's own
;;; convention said in the one alphabet a leader sequence has.
(define-key "org-mode" "SPC m H" "org-promote-subtree")
(define-key "org-mode" "SPC m L" "org-demote-subtree")
(define-key "org-mode" "SPC m T" "org-insert-todo-heading")
