;;;; show-paren.lisp — the bracket at point, and the one it pairs with.
;;;;
;;;; Emacs' `show-paren-mode', and the same twenty-year-old observation behind
;;;; it: the moment you finish typing `)' the only question you have is *which*
;;;; `(' it closed, and the editor is the only thing in the room that already
;;;; knows. Answering costs one scan and saves you counting brackets down a
;;;; screenful of Lisp.
;;;;
;;;; Not "rainbow parentheses", which is the other feature with a similar name:
;;;; that colours every pair by nesting depth, all the time, whether or not you
;;;; are near one. This lights up exactly two characters and only while point is
;;;; on one of them, which is the version that answers a question rather than
;;;; decorating the page. Rainbow needs a colour per depth and there are no
;;;; faces to spend on it — see the note on colours below.
;;;;
;;;; Every buffer, not just Lisp: `(`, `[` and `{` are brackets in Python, Rust,
;;;; JSON and prose, and a feature that asked the major mode first would be off
;;;; in most of the places it helps.
;;;;
;;;; The matching itself is one primitive, `matching-bracket', in
;;;; `crates/core/src/query.rs' — a scan of the rope with a depth counter, which
;;;; is exactly the shape of thing that belongs on the Rust side of the boundary.
;;;; `%' in the evil grammar asks the same function, so where the highlight
;;;; points and where `%' would jump can never disagree.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; How it looks
;;;
;;; A face and a weight, rather than a background, and the reason is the face
;;; list. An overlay's colours are *named* from `face-list' so that loading a
;;; theme recolours every overlay for free — which is the right trade and is
;;; also the whole constraint here, because there is no `paren-match' face in
;;; that list and Lisp cannot add one. Spending `keyword' on it with
;;; `set-syntax-color' would change what every keyword in every buffer looks
;;; like, to light up two characters.
;;;
;;; So: the theme's own `keyword' colour, plus real bold. Both are already in
;;; the vocabulary, both follow a theme change, and bold is what carries the
;;; highlight in a theme whose keyword colour happens to sit near the text
;;; colour. `*show-paren-face*' is NIL-able for anyone who wants weight alone.
(defparameter *show-paren-face* "keyword"
  "Face name the matched pair is drawn in, or NIL to leave the colour alone.")

(defparameter *show-paren-bold* t
  "Whether the matched pair is drawn bold as well as coloured.")

(defvar *show-paren-overlays* nil
  "The two overlays this file drew, so it takes its own off and nobody else's.")

(defvar *show-paren-at* nil
  "The (POINT . BUFFER) the current highlight was drawn for.
Point moves far more often than it moves *onto a bracket*, so the common case
has to be a comparison rather than two overlay deletions and a scan.")

;;; The character at POS, or NIL past the end. `buffer-substring' is the only
;;; reader that answers about one, so this is a one-line adaptor rather than a
;;; new primitive. `%'-prefixed because it is this file's, not a general one.
(defun %show-paren-char (pos)
  (let ((s (and (< pos (point-max)) (buffer-substring pos (1+ pos)))))
    (and s (plusp (length s)) (char s 0))))

(defun %show-paren-clear ()
  (dolist (ov *show-paren-overlays*) (ignore-errors (delete-overlay ov)))
  (setf *show-paren-overlays* nil))

(defun %show-paren-mark (pos)
  "One character at POS, lit. NIL when there is no character there."
  (let ((ov (and (< pos (point-max)) (make-overlay pos (1+ pos)))))
    (when ov
      (when *show-paren-face* (overlay-put ov 'face *show-paren-face*))
      (when *show-paren-bold* (overlay-put ov 'weight 'bold))
      ;; Not read by the renderer — it is how these are told apart from anyone
      ;; else's overlays, the way `folds-in' tells folds from org's bullets.
      (overlay-put ov 'show-paren t))
    ov))

(defun show-paren-update ()
  "Light the bracket at point and its partner, or take the last pair off.

On `*point-moved-functions*'. Cheap on the overwhelmingly common keystroke —
one integer compare — because the highlight only has to be rebuilt when point
lands somewhere new, and it lands off a bracket nearly every time."
  (let ((here (cons (point) (buffer-name))))
    (unless (equal here *show-paren-at*)
      (setf *show-paren-at* here)
      (%show-paren-clear)
      ;; `matching-bracket' answers for the character at point *and* for the one
      ;; just before it, which is the position point is actually in the instant
      ;; you finish typing a closing bracket — the case this exists for.
      (let ((other (matching-bracket (point))))
        (when other
          ;; Which end point is on decides which character to light beside the
          ;; answer: on an opener it is point itself, and just past a closer it
          ;; is the character behind point.
          (let ((mine (if (and (plusp (point))
                               (> (point) other)
                               (not (find (%show-paren-char (point)) ")]}")))
                          (1- (point))
                          (point))))
            (setf *show-paren-overlays*
                  (remove nil (list (%show-paren-mark mine)
                                    (%show-paren-mark other))))))))))

(defvar *point-moved-functions* nil)
(pushnew 'show-paren-update *point-moved-functions*)

;;; A change can move the pair without moving point — typing `(` in front of an
;;; existing one, or an edit arriving from Lisp — so the change hook forces a
;;; rebuild by forgetting where the highlight was drawn. Not a second scan: it
;;; invalidates, and the mover does the work on the next report.
(defvar *after-change-functions* nil)
(defun %show-paren-invalidate () (setf *show-paren-at* nil))
(pushnew '%show-paren-invalidate *after-change-functions*)
