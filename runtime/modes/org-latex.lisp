;;;; org-latex-preview — `$x^2$' drawn rather than spelled.
;;;;
;;;; Written here, in Lisp, and that is the point. Rust contributes exactly two
;;;; things it alone can do: `latex-fragments' scans the buffer for `$...$',
;;;; `\[...\]' and `\begin{env}...\end{env}', and `latex-preview' runs one
;;;; fragment through latex -> DVI -> dvipng and answers an image handle. The
;;;; policy — which fragments, what to do with the old ones, what to say
;;;; afterwards — is all below, where you can change it.
;;;;
;;;; An overlay is a range that moves with the text plus a property list, and the
;;;; properties the renderer draws are `face', `background', `display' and
;;;; `image'. Anything else you put on one stays in this image and can be any
;;;; Lisp object at all, which is what `:latex' is being used for here: a mark
;;;; saying "this one is mine", so re-previewing replaces its own overlays and
;;;; leaves anybody else's alone.
;;;;
;;;; A cold render is a few hundred milliseconds *per fragment* and it happens on
;;;; the Lisp thread — so the editor keeps drawing and keeps taking your
;;;; keystrokes while a screenful of equations is typeset, and only the image
;;;; queues behind it. Warm, from the on-disk cache, the whole buffer is instant.
;;;;
;;;; This lived in `init.lisp' while it was the only org feature and the config
;;;; was the shortest place to put it. It is a *feature*, and the runtime is
;;;; where org's features are — beside `org-modern.lisp', which draws the rest of
;;;; the markup, and `org-fold.lisp', which folds it. The keys are still the
;;;; config's: `init.lisp' binds `C-c r' and `SPC m l'.
;;;;
;;;; Loaded before `org-modern.lisp', whose `org-mode' body — the last one
;;;; declared in the runtime, and therefore the only one that runs — calls
;;;; `org-latex-preview-new' so a file *arrives* typeset.

(in-package :zemacs)

(defun org-latex-previews (beg end)
  "Handles of the preview overlays this file made, overlapping BEG..END."
  (remove-if-not (lambda (o) (overlay-get o :latex))
                 (mapcar #'first (overlays-in beg end))))

(defun org-latex-preview-clear ()
  "Take the previews off, showing the LaTeX source again."
  (let ((ovs (org-latex-previews (point-min) (point-max))))
    (mapc #'delete-overlay ovs)
    (message (format nil "~d preview~:p cleared" (length ovs)))))

(defvar *org-latex-auto* t
  "Whether org buffers typeset their fragments by themselves.

Turned off by a pass that could not render anything — a machine with no `latex'
should say so once, not once per equation — and back on by `org-latex-preview'
succeeding, since asking by hand is how you say you have fixed it.")

(defun %org-latex-draw (fbeg fend)
  "Typeset the fragment between FBEG and FEND and hang an overlay on it. T when
LaTeX produced an image, NIL when it could not — which is the answer every
caller branches on, because it is the difference between `this equation is
wrong' and `this machine has no latex'."
  (let ((image (latex-preview (buffer-substring fbeg fend))))
    (when image
      (let ((ov (make-overlay fbeg fend)))
        (when ov
          (overlay-put ov :latex t)
          (overlay-put ov 'image image)))
      t)))

(defun %org-latex-render (beg end)
  "Preview every fragment between BEG and END, answering how many were drawn, or
NIL when one of them could not be rendered at all. Stops at the first failure:
a hundred identical `latex: not found' messages tell you nothing the first did.

Back to front: an overlay adjusts itself across an edit, but nothing here edits,
and walking backwards keeps the *offsets* from `latex-fragments' valid however
long the rendering takes."
  (let ((done 0))
    (dolist (f (reverse (latex-fragments)) done)
      (destructuring-bind (fbeg fend display) f
        (declare (ignore display))
        (when (and (< fbeg end) (> fend beg))
          (if (%org-latex-draw fbeg fend)
              (incf done)
              ;; NIL out of the DOLIST, which is this function's value.
              (return nil)))))))

(defun %org-latex-fragment-at-point ()
  "(BEG . END) of the fragment point is inside, or NIL.

`latex-fragments' scans the whole buffer and there is no reader for \"the one
here\", but the list it answers is short and already in order — so finding point
in it is a walk over a handful of pairs rather than a second pass over the text,
and no new primitive.

Both delimiters count as inside. Point on the closing `$' of `$x^2$' is in that
equation to anyone who just typed it, and a rule that said otherwise would make
`C-c r' silently do the whole buffer from the one position you are most likely
to press it from."
  (let ((p (point)))
    (dolist (f (latex-fragments))
      (destructuring-bind (fbeg fend display) f
        (declare (ignore display))
        (when (and (<= fbeg p) (<= p fend))
          (return (cons fbeg fend)))))))

(defun org-latex-preview ()
  "Show LaTeX fragments as images: the selection's, the one point is inside, or
— failing both — the whole buffer's.

The middle case is the one that makes this a command you press rather than one
you schedule. Inside `$...$' or a `\\begin{...}' block, `C-c r' renders *that*
equation: a few hundred milliseconds, against a few hundred per fragment for a
file full of them. It is also what you mean by pressing it there — you are
looking at one equation, and the buffer is not what you were asking about.

Fragments already previewed are re-done, so this doubles as `refresh' at
whichever of the three scopes it picked."
  (let* ((r (or (region) (%org-latex-fragment-at-point)))
         (beg (if r (car r) (point-min)))
         (end (if r (cdr r) (point-max))))
    (mapc #'delete-overlay (org-latex-previews beg end))
    (let ((done (%org-latex-render beg end)))
      ;; Asking by hand also *re-arms* the automatic pass: the usual reason a
      ;; machine had no `latex' is that it has one now.
      (setf *org-latex-auto* (and done t))
      (message (if done
                   (format nil "~d fragment~:p previewed" done)
                   "latex: nothing previewed — automatic previews off")))))

;;; ---------------------------------------------------------------------------
;;; ...and previewing without being asked
;;;
;;; The command above is the whole mechanism; this is the policy that decides
;;; when to run it, and the policy is entirely about *cost*. A cold render is a
;;; few hundred milliseconds per fragment and `latex-fragments' is a pass over
;;; the buffer, so the one thing this must never do is either of them per
;;; keystroke.
;;;
;;; Two triggers, and between them they are what "first-class inline LaTeX"
;;; means in practice:
;;;
;;;   entering org-mode   — the buffer arrives already typeset. Cold this costs
;;;                         one render per fragment on the Lisp thread while the
;;;                         editor keeps taking your keystrokes; warm, from the
;;;                         on-disk cache, it is instant. `org-modern.lisp' is
;;;                         where that call lives, because its `org-mode' body is
;;;                         the last one declared and therefore the only one that
;;;                         runs.
;;;   leaving the line    — you finish editing `$\alpha$', move off the line,
;;;   you were editing      and it becomes an image. Which is exactly when you
;;;                         want it: rendering *while* you type would spend a
;;;                         latex run on `$\alph', `$\alpha', `$\alpha$' in turn
;;;                         and flicker an image in and out under the cursor.
;;;
;;; The line test is what makes the second one affordable. `after-change-hook'
;;; only records that something changed and which line it was — two variables,
;;; no scan — and `point-moved-hook' does the work only once the two disagree.
;;; Typing therefore costs a comparison per keystroke, and the buffer pass
;;; happens once per line you edit rather than once per character.

(defvar *org-latex-edited-line* nil
  "The line an edit last touched, or NIL when nothing is waiting to be typeset.
NIL is also the whole of the dirty flag: there is no second variable.")

(defun %org-latex-previewed-ranges ()
  "(BEG . END) of every preview overlay in the buffer, in one query.

`overlays-in' already answers (ID BEG END), so asking once for the whole buffer
and matching in the image costs one round trip; asking per fragment — which is
what `org-latex-previews' does — would cost one per equation on a hook."
  (let ((out nil))
    (dolist (o (overlays-in (point-min) (point-max)) (nreverse out))
      (when (overlay-get (first o) :latex)
        (push (cons (second o) (third o)) out)))))

(defun org-latex-preview-new ()
  "Typeset the fragments that have no preview yet, quietly.

Two queries for the whole pass — the fragments and the overlays — and then a
render only for what is genuinely new. That is what makes this cheap enough to
hang off a hook: a buffer whose equations are all drawn already costs those two
and no LaTeX at all.

Quietly matters too: a `3 fragments previewed' in the status line every time you
leave a line would be the editor talking over you. Only a *failure* is worth a
message, and only the once."
  (when (and *org-latex-auto* (derived-mode-p 'org-mode))
    (let ((have (%org-latex-previewed-ranges))
          (at (point)))
      (dolist (f (reverse (latex-fragments)))
        (destructuring-bind (fbeg fend display) f
          (declare (ignore display))
          (when (and
                 ;; Already drawn. Overlap and not containment: an overlay
                 ;; shifts with the text around it, so a fragment whose source
                 ;; has grown by a character is still the same equation.
                 (notany (lambda (r) (and (< (car r) fend) (> (cdr r) fbeg))) have)
                 ;; ...and not the one point is inside: you are still typing in
                 ;; it, and `$\alph' is a LaTeX error, not an equation.
                 (not (and (<= fbeg at) (<= at fend))))
            (unless (%org-latex-draw fbeg fend)
              (setf *org-latex-auto* nil)
              (message "latex: automatic previews off — `SPC m l' to retry")
              (return)))))))
  nil)

(defun org-latex-note-change ()
  "Remember that this line now wants typesetting. On `after-change-hook', so it
must stay this cheap: one reader and one SETF, no scan."
  (when (derived-mode-p 'org-mode)
    (setf *org-latex-edited-line* (line-number))))

(defun org-latex-maybe-preview ()
  "Typeset the edited line's fragments once point has left it.

On `point-moved-hook'. The guard is two integers, so navigating a buffer nobody
has edited costs one comparison per keystroke and nothing else."
  (when (and *org-latex-edited-line*
             (/= *org-latex-edited-line* (line-number)))
    (setf *org-latex-edited-line* nil)
    (org-latex-preview-new))
  nil)

(add-hook '*after-change-functions* 'org-latex-note-change)
(add-hook '*point-moved-functions* 'org-latex-maybe-preview)
