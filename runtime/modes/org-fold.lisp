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
