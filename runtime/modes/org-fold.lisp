;;;; Code folding — the policy half.
;;;;
;;;; Rust owns exactly one fact: an overlay carrying `fold' makes the lines
;;;; after its first one stop occupying rows. The renderer does not draw them
;;;; and `j' steps over them. That is the whole hole, and it is the one thing
;;;; overlays could not already do — every other payload replaces cells with
;;;; cells, and this makes rows cease to exist.
;;;;
;;;; Everything about *what* is foldable is here, in Lisp, because it is exactly
;;;; the part every config bends: an org subtree, a `defun', a brace block, a
;;;; magit hunk. `*fold-subtree-functions*' is the hook — one entry and one
;;;; function teaches a new mode to fold, with no rebuild.
;;;;
;;;; The library underneath (`fold-region', `folds-in', `folded-p',
;;;; `unfold-region', `unfold-all') is in the shim beside the other overlay
;;;; helpers, since a fold *is* an overlay: it moves with the text you typed
;;;; above it, dies with the text it covered, and comes off with
;;;; `delete-overlay' like anything else.
;;;;
;;;; ponytail: folding is two-state, not org's three. `org-cycle' in Emacs walks
;;;; FOLDED -> CHILDREN -> SUBTREE; here a headline is closed or open, and
;;;; opening one opens everything under it. CHILDREN is a third case in
;;;; `fold-dwim' plus a per-headline state table, and nobody has missed it yet.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; org

(defun %org-level (line)
  "LINE's headline level — its run of leading `*' — or NIL when it is not one.

A headline is stars at column 0 followed by a space, which is org's own rule and
the same one `crates/syntax/src/org.rs' applies when it colours them. Answering
NIL for `**bold**' at the start of a line is the point of the space test."
  (let* ((text (line-string line))
         (n (or (position-if-not (lambda (c) (char= c #\*)) text) (length text))))
    (when (and (plusp n) (< n (length text)) (char= (char text n) #\Space))
      n)))

(defun %org-headline-above (&optional line)
  "The nearest headline at or above LINE (default the line point is on), or NIL
when point is in the preamble before the first one."
  (loop for l from (or line (line-number)) downto 1
        when (%org-level l) return l))

(defun %org-subtree-end (line)
  "Char offset of the last character of the subtree headed by LINE.

The subtree runs until a headline of the *same or shallower* level, which is
org's definition and what makes folding a `**' leave the `*' it lives under
alone."
  (let ((level (%org-level line))
        (last line))
    (loop for l from (1+ line) to (line-count)
          do (let ((lv (%org-level l)))
               (when (and lv (<= lv level)) (return))
               (setf last l)))
    (line-end last)))

(defun org-subtree-at-point ()
  "(BEG . END) for the org subtree under point, or NIL outside one.
The shape `*fold-subtree-functions*' expects."
  (let ((line (%org-headline-above)))
    (when line (cons (line-start line) (%org-subtree-end line)))))

(defun org-subtrees ()
  "(BEG . END) for every top-level subtree — org's `overview' state."
  (loop for l from 1 to (line-count)
        when (eql (%org-level l) 1)
          collect (cons (line-start l) (%org-subtree-end l))))

(defun %org-children (line)
  "Lines of LINE's *direct* children — headlines exactly one level deeper.

Direct and not descendant: a grandchild lives inside a child's own subtree and
is that child's business to reveal, which is the whole difference between org's
CHILDREN state and simply unfolding everything."
  (let ((level (%org-level line))
        (out '()))
    (loop for l from (1+ line) to (line-count)
          do (let ((lv (%org-level l)))
               (when (and lv (<= lv level)) (return))
               (when (eql lv (1+ level)) (push l out))))
    (nreverse out)))

;;; ---------------------------------------------------------------------------
;;; org's own three-state cycle
;;;
;;; The ceiling at the top of this file, now closed. `fold-dwim' is still the
;;; generic two-state toggle every mode gets — a selection, a defun, a magit
;;; hunk — and this is the one mode whose folding is a *structure* rather than a
;;; range, so it is the one mode that earns a third state.
;;;
;;; What makes CHILDREN expressible without a new primitive: a fold hides the
;;; lines *after* the first line of its range. So "headline visible, body
;;; hidden" is one fold from the headline to the line before its first child,
;;; and each child is the same shape one level down. CHILDREN is therefore a
;;; *set* of ordinary folds, not a new kind of one, and everything that already
;;; knows how to unfold keeps working on it.
;;;
;;; ponytail: the state is read back off the buffer every press rather than
;;; remembered per headline. That is one `folded-p' per direct child, which is
;;; nothing next to a redisplay, and it has the property a table would not: a
;;; fold you removed by hand, or one that died with the text under it, cannot
;;; leave the cycle believing something the buffer disagrees with.

(defun %org-cycle-state (beg end)
  "Which of :SUBTREE, :FOLDED or :CHILDREN the subtree BEG..END is showing.

Told apart by the folds' *extents*, which is the only thing that distinguishes
the last two: both states have a fold starting at the headline. FOLDED is the
single fold that reaches the end of the subtree; CHILDREN is a body fold that
stops before the first child, plus one per child, none of which reaches the end.

Not `folded-p': that answers \"the innermost fold covering this position\", and
in CHILDREN state every child's headline is the *start* of its own fold — so
asking there returns that fold and reports CHILDREN as FOLDED. The cycle then
sticks between the two and never opens. `>=' rather than `=' on the far edge
because an overlay adjusts itself across an edit and may have grown."
  (let ((folds (folds-in beg end)))
    (cond ((null folds) :subtree)
          ((some (lambda (ov)
                   (and (= (overlay-start ov) beg) (>= (overlay-end ov) end)))
                 folds)
           :folded)
          (t :children))))

(defun %org-show-children (line beg end)
  "Reveal LINE's direct children and hide everything else under it."
  (unfold-region beg end)
  ;; A leaf headline has no middle state, so opening it *is* the whole of this
  ;; and the cycle is two-state there. Guarded rather than special-cased by the
  ;; caller because `(first NIL)' is NIL and every arithmetic test below would
  ;; then be a type error on the one shape that reaches it.
  (let ((kids (%org-children line)))
    (when kids
      ;; The body between the headline and its first child. Skipped when the
      ;; child follows immediately: a fold spanning a single line hides nothing
      ;; and would only be one more overlay to read back.
      (let ((first-kid (first kids)))
        (when (> first-kid (1+ line))
          (fold-region beg (line-end (1- first-kid)))))
      (dolist (k kids)
        (fold-region (line-start k) (%org-subtree-end k))))))

(defun org-cycle ()
  "TAB on a headline: SUBTREE -> FOLDED -> CHILDREN -> SUBTREE.

org's own cycle and org's own order — pressing TAB on something open closes it,
which is the gesture that makes an outline an outline. A headline with no
children has no middle state and cycles in two.

Off a headline this defers to `fold-dwim', so TAB in the preamble or over a
selection still does the generic thing rather than reporting that there is no
subtree here."
  (let ((line (%org-headline-above)))
    (if (null line)
        (fold-dwim)
        (let* ((beg (line-start line))
               (end (%org-subtree-end line)))
          (ecase (%org-cycle-state beg end)
            (:subtree  (fold-region beg end)          (message "folded"))
            (:folded   (%org-show-children line beg end) (message "children"))
            (:children (unfold-region beg end)        (message "subtree")))))))

;;; ---------------------------------------------------------------------------
;;; tree-sitter — which is every language, and needs no entry anywhere
;;;
;;; org has a hand-written idea of a subtree above because org has no grammar in
;;; this build. Everything that *does* have one already gets parsed on every
;;; keystroke to be coloured, and `fold-ranges' asks that same tree a different
;;; question: which line ranges are structural. So folding a `defun', a class, a
;;; brace block or a JSON object costs no per-language work at all — the mode
;;; list below this is empty on purpose, and adding a grammar to the build adds
;;; its folding with it.
;;;
;;; The reader answers *every* named node spanning more than one line, outermost
;;; first, and picking among them is the whole of the policy here: the innermost
;;; range for "fold what I am in", the outermost ones for "show me the shape of
;;; the file". Two `loop's, and Rust needed no opinion about either.
;;;
;;; ponytail: no `folds.scm'. A fold query per grammar is more precise — it is
;;; how Neovim does it — and it is a file per language to write and keep in step
;;; with upstream. The imprecision it buys is that a multi-line argument list is
;;; offered as a fold too, which costs a press of `z a' landing on something
;;; smaller than you meant and never costs a wrong fold.
;;;
;;; ponytail: `fold-ranges' re-parses the buffer on each call, where the
;;; highlighter next door is doing an incremental parse of the same text. It runs
;;; on this thread and nothing waits on it, and the press that asks is one you
;;; made deliberately — so the ceiling is a big file and a held-down `z a'. The
;;; upgrade is reaching the highlighter's `Session' tree, which today lives on
;;; the worker thread with no way to ask it a second question.

(defun tree-sitter-subtree-at-point ()
  "(BEG . END) for the innermost structural range covering point, or NIL.

Innermost, so pressing the key inside a method folds the method rather than the
class it lives in — and `fold-ranges' answers outermost first, so the innermost
match is simply the last one that covers this line."
  (let ((line (line-number))
        (best nil))
    (loop for (a b) in (fold-ranges)
          when (and (<= a line) (<= line b)) do (setf best (cons a b)))
    (when best
      (cons (line-start (car best)) (line-end (cdr best))))))

(defun tree-sitter-subtrees ()
  "Every *outermost* structural range in the buffer — `fold-all''s shape.

Outermost and not all of them: `fold-all' means \"show me the shape of this
file\", which is one fold per top-level definition. A nested range would be a
fold inside a fold nobody can see, and `fold-open-all' would then need two
passes to undo what one press did.

The ranges arrive outermost first, so anything starting at or before the last
range's end is inside it — one integer of state and no interval arithmetic."
  (let ((end 0)
        (out '()))
    (loop for (a b) in (fold-ranges)
          when (> a end)
            do (setf end b)
               (push (cons (line-start a) (line-end b)) out))
    (nreverse out)))

;;; ---------------------------------------------------------------------------
;;; the generic commands

(defparameter *fold-subtree-functions*
  '(("org-mode" . org-subtree-at-point))
  "Major mode -> a function answering (BEG . END) for the foldable thing under
point, or NIL. *This is the policy hook*, and it is an override rather than a
requirement: a mode with no entry falls through to `tree-sitter-subtree-at-point',
so folding works in every language the build has a grammar for without anyone
adding a line. org is here because org has no grammar and a headline is not a
syntax node.")

(defparameter *fold-all-functions*
  '(("org-mode" . org-subtrees))
  "Major mode -> a function answering every foldable (BEG . END) in the buffer.
Read by `fold-all' only, and separate from `*fold-subtree-functions*' because
\"every top-level heading\" is not \"the heading under point\" applied N times.
Falls through to `tree-sitter-subtrees' the same way.")

(defun %fold-fn (table fallback)
  "The function TABLE names for this major mode, else FALLBACK."
  (let ((f (cdr (assoc (major-mode) table :test #'string=))))
    (if (and f (fboundp f)) f fallback)))

(defun %fold-range ()
  "The range `fold-dwim' should act on: the selection if there is one, else
whatever this major mode calls a subtree, else what the parser calls one."
  (or (region) (funcall (%fold-fn *fold-subtree-functions*
                                  'tree-sitter-subtree-at-point))))

(defun fold-dwim ()
  "Fold or unfold what is under point.

The selection when there is one — so folding an arbitrary block needs no mode
support at all — and otherwise the mode's own subtree. Toggling opens
*everything* inside the range, which is org's SUBTREE state rather than its
CHILDREN one; see the ceiling at the top of this file."
  (let ((r (%fold-range)))
    (cond ((null r) (message "nothing foldable here"))
          ((plusp (unfold-region (car r) (cdr r))) (message "unfolded"))
          (t (fold-region (car r) (cdr r))
             (message "folded")))))

(defun fold-all ()
  "Fold every foldable range in the buffer — org's `overview'.

Opens what is already folded first, so pressing it twice does not stack a second
overlay over the first and leave `fold-dwim' needing two presses to undo."
  (unfold-all)
  (let ((ranges (funcall (%fold-fn *fold-all-functions* 'tree-sitter-subtrees))))
    (dolist (r ranges) (fold-region (car r) (cdr r)))
    (message (format nil "~a fold~:p" (length ranges)))))

(defun fold-open-all ()
  "Open every fold in the buffer."
  (message (format nil "~a fold~:p opened" (unfold-all))))

;;; TAB on a headline, which is *the* org gesture and the reason the cycle above
;;; exists at all. Mode-local, so it is org's and nothing else's: `lisp-mode'
;;; keeps `<tab>' for indentation and magit keeps it for its sections.
(define-mode-key 'org-mode "<tab>" "org-cycle")

;;; vim's own fold keys, which is the muscle memory this is for. `z' is not a
;;; prefix in the built-in grammar and is bound only inside magit, so these
;;; cost nothing anywhere else.
(define-leader "z a" "fold-dwim")
(define-leader "z M" "fold-all")
(define-leader "z R" "fold-open-all")
;;; ...and under the leader too, since that is where which-key will show them.
(define-leader "SPC t f" "fold-dwim")
(define-leader "SPC t F" "fold-all")
(define-leader "SPC t u" "fold-open-all")
