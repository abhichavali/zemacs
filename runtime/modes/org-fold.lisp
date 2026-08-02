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
;;; the generic commands

(defparameter *fold-subtree-functions*
  '(("org-mode" . org-subtree-at-point))
  "Major mode -> a function answering (BEG . END) for the foldable thing under
point, or NIL. *This is the policy hook.* Teaching another mode to fold is one
entry here and one function; there is nothing to add in Rust, because Rust has
no opinion about what a subtree is.")

(defparameter *fold-all-functions*
  '(("org-mode" . org-subtrees))
  "Major mode -> a function answering every foldable (BEG . END) in the buffer.
Read by `fold-all' only, and separate from `*fold-subtree-functions*' because
\"every top-level heading\" is not \"the heading under point\" applied N times.")

(defun %fold-range ()
  "The range `fold-dwim' should act on: the selection if there is one, else
whatever this major mode calls a subtree."
  (or (region) (let ((f (cdr (assoc (major-mode) *fold-subtree-functions*
                                    :test #'string=))))
                 (and f (fboundp f) (funcall f)))))

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
  (let ((f (cdr (assoc (major-mode) *fold-all-functions* :test #'string=))))
    (cond ((not (and f (fboundp f))) (message "nothing to fold in this mode"))
          (t (unfold-all)
             (let ((ranges (funcall f)))
               (dolist (r ranges) (fold-region (car r) (cdr r)))
               (message (format nil "~a fold~:p" (length ranges))))))))

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
