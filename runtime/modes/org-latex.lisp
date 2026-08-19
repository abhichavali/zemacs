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
;;;; config's: `init.lisp' binds `C-c r' and `SPC m e'.
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
    ;; ...and disarm the automatic pass, or it undoes this on your next
    ;; keystroke. Both flags below mean "something is owed a render", and you
    ;; press this from precisely where one of them is set — inside the equation
    ;; you walked into, or on the line you were just typing on — so the first
    ;; movement after `C-c R' used to draw the whole buffer again. Nothing is
    ;; owed any more: you have just said you want the source.
    (setf *org-latex-edited-line* nil
          *org-latex-revealed* nil)
    (message (format nil "~d preview~:p cleared" (length ovs)))))

(defvar *org-latex-auto* t
  "Whether org buffers typeset their fragments by themselves.

Turned off by an asked-for pass that could not render *anything* — which is what
a machine with no `latex' on it looks like from in here — and back on by the
next one that renders something, since asking by hand is how you say you have
fixed it. One equation LaTeX refuses is not that pass and never touches this:
see `*org-latex-failed*'.")

(defvar *org-latex-failed* nil
  "Fragment sources LaTeX has already turned down.

A fragment that will not compile is the *fragment's* problem, and it used to be
the whole document's: the first `\\fract{1}{2}' stopped the pass and switched
`*org-latex-auto*' off, so one typo left every other equation in the file
spelled out behind a message blaming your machine for it. Remembered here
instead, so the bad one costs a single LaTeX run, shows its source — which is
the right thing to show for an equation that does not typeset — and everything
around it still draws.

Keyed on the text and not on the offsets, because the offsets move and the text
is what LaTeX objected to: fixing the typo makes a different string and it is
tried afresh with no bookkeeping. The size is deliberately not part of the key
either — a LaTeX error is not a function of the dpi.

Forgotten by `org-latex-preview': asking by hand is how you say you have fixed
it, and that has to include fixing it by installing TeX.")

(defparameter *org-latex-display-scale* 1.25
  "How much larger a display fragment is set than an inline one.

`$x^2$' is a word in a sentence and has to sit on the line at the size of the
words either side of it. `\\[ … \\]' is not: it has a line of its own, it is
what the paragraph is *about*, and a typeset document sets it larger for that
reason — the same reason a heading is larger than the body. This is the whole of
that distinction, and `latex-fragments' already answers which kind a fragment is.

1 makes them the same size again.")

(defun %org-latex-heading-scales ()
  "org-modern's type size per heading level, or NIL when it is not loaded.

Read through the symbol rather than named directly, because this file loads
*before* `org-modern.lisp' — see the head of this file — and a config that
switched org-modern off must leave equations working rather than unbound."
  (and (boundp '*org-modern-heading-scale*)
       (symbol-value '*org-modern-heading-scale*)))

(defun %org-latex-scale (text at display)
  "The size to set the fragment at offset AT in TEXT, as a multiple of the body.

Two independent factors and they multiply:

  * **where it is.** A fragment in a heading is part of that heading and has to
    be the heading's size, or `* The $\\epsilon$ argument' reads as a title with
    a footnote dropped into the middle of it. The level is counted off the
    leading stars of the line AT is on and sized by org-modern's own list, so a
    config that resizes its headings resizes the equations in them too.
  * **what it is.** Display or inline — see `*org-latex-display-scale*'.

TEXT is the whole buffer, passed in rather than read here: this is called once
per fragment being drawn and `buffer-string' is a copy of the document."
  (* (let* ((limit (length text))
            (from (min at limit))
            ;; The start of AT's line. `position :from-end' over a prefix, which
            ;; is the one thing there is no reader for — `line-start' takes a
            ;; line *number* and nothing converts an offset into one.
            (bol (let ((nl (position #\Newline text :end from :from-end t)))
                   (if nl (1+ nl) 0)))
            (after (or (position #\* text :start bol :test #'char/=) limit))
            (stars (- after bol)))
       ;; A heading is stars *then a space*: `**bold**' at the start of a line is
       ;; not a level-2 heading, and sizing it as one would blow up every
       ;; equation in a paragraph that happens to open with emphasis.
       (if (and (plusp stars) (< after limit) (char= (char text after) #\Space))
           (or (nth (1- stars) (%org-latex-heading-scales)) 1)
           1))
     (if display *org-latex-display-scale* 1)))

(defun %org-latex-draw (fbeg fend &optional (scale 1))
  "Typeset the fragment between FBEG and FEND and hang an overlay on it. T when
an image went up, NIL when LaTeX would not give one — which is the answer every
caller counts, because it is the difference between an equation that is drawn
and one still spelled out.

A source LaTeX has refused before is not asked again — see `*org-latex-failed*'
— so this answers NIL for it without a run, and a bad equation costs one latex
invocation for the whole session rather than one per pass.

SCALE is a multiple of the body em — see `%org-latex-scale'. It reaches the
renderer as a dpi rather than as a transform on the bitmap, so a heading's
equation is *typeset* large instead of being a small one scaled up."
  (let* ((source (buffer-substring fbeg fend))
         (image (unless (member source *org-latex-failed* :test #'string=)
                  (or (latex-preview source scale)
                      ;; `latex-preview' has already said why in the status
                      ;; line. All this adds is "and do not ask again".
                      (progn (push source *org-latex-failed*) nil)))))
    (when image
      (let ((ov (make-overlay fbeg fend)))
        (when ov
          (overlay-put ov :latex t)
          (overlay-put ov 'image image)
          ;; A `\begin{align}' spans several lines and the image draws on the
          ;; first of them. Without this the others stay as the blank rows their
          ;; source became — the equation sitting at the top of a hole as deep as
          ;; the source was long, which is the "too much space" you see under a
          ;; display fragment and never under `$x^2$'.  `:fold' makes those rows
          ;; stop existing rather than stop having text, and `j' steps over them.
          ;;
          ;; **Only when there are rows to fold.** This used to be set on every
          ;; preview on the argument that it was a no-op for a one-line fragment,
          ;; and it was not: a fold is what the renderer draws its `…' for, so
          ;; every line holding an inline `$x^2$' grew an ellipsis on the end
          ;; claiming there was text behind it. There was none — the flag was
          ;; being used to claim rows the fragment did not have.
          (when (find #\Newline source)
            (overlay-put ov :fold t))))
      t)))

(defun %org-latex-render (beg end)
  "Preview every fragment between BEG and END, answering how many were drawn.

*Every* one, and a failure is not a reason to abandon the rest. It used to be —
the pass returned NIL at the first fragment LaTeX refused, on the argument that
a hundred identical `latex: not found' messages tell you nothing the first did.
True of a machine with no TeX and false of a document with a typo in it, and the
two are indistinguishable from here; `*org-latex-failed*' is what makes stopping
unnecessary, since each source is only ever asked once either way.

Back to front: an overlay adjusts itself across an edit, but nothing here edits,
and walking backwards keeps the *offsets* from `latex-fragments' valid however
long the rendering takes."
  (let ((done 0)
        ;; One copy of the document for the whole pass, and only when there is a
        ;; fragment to size — `%org-latex-scale' needs the line a fragment sits
        ;; on and there is no reader for that. A buffer with no equations in it
        ;; pays nothing.
        (text nil))
    (dolist (f (reverse (latex-fragments)) done)
      (destructuring-bind (fbeg fend display) f
        (when (and (< fbeg end) (> fend beg))
          (unless text (setf text (buffer-string)))
          (when (%org-latex-draw fbeg fend (%org-latex-scale text fbeg display))
            (incf done)))))))

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
    ;; Asking by hand is how you say you have fixed it, so the refusals go: the
    ;; typo you just corrected, and the TeX you just installed, are both `try
    ;; these again'. Only by hand — an automatic pass that forgot would run
    ;; LaTeX over the same broken equation every time you left a line.
    (setf *org-latex-failed* nil)
    (mapc #'delete-overlay (org-latex-previews beg end))
    (let ((done (%org-latex-render beg end)))
      ;; Off only when LaTeX was asked and refused *every* time, which is what a
      ;; machine with no `latex' looks like from here. A buffer with no equations
      ;; in it has told you nothing about your machine and must not switch
      ;; anything off; nor must one bad equation among four good ones. Asking by
      ;; hand also *re-arms* the automatic pass: the usual reason a machine had
      ;; no `latex' is that it has one now.
      (setf *org-latex-auto* (not (and (zerop done) *org-latex-failed*)))
      (message (if *org-latex-auto*
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
  "The line an edit last touched, or NIL when nothing is waiting to be typeset.")

(defvar *org-latex-revealed* nil
  "(BEG . END) of the fragment point walked into, or NIL.

The second flag, and it is not a duplicate of the line above. A *line* is the
right unit for an edit — rendering per keystroke would spend a latex run on
`$\\alph' and flicker an image under the cursor — and it is the wrong unit
for a *walk*: `$a$ and $b$' is two equations on one line, so stepping off the
first onto the word between them left the line unchanged and left `$a$' spelled
out until you pressed `j'. Which is exactly the previews-sometimes this is here
to fix.

Cleared by an edit rather than adjusted by one: typing inside a revealed
fragment moves its end, and a stale end would make point look as though it had
walked out and re-typeset the equation mid-word. The line flag owns that case
and always did.")

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
leave a line would be the editor talking over you, and a failure has already
said its piece from inside `latex-preview' — once per source, and never twice
for the same one, which is `*org-latex-failed*''s whole job."
  (when (and *org-latex-auto* (derived-mode-p 'org-mode))
    (let ((have (%org-latex-previewed-ranges))
          (at (point))
          ;; Fetched on the first fragment that actually needs drawing — see
          ;; `%org-latex-render'. This runs every time you leave a line, and the
          ;; overwhelmingly common answer is "everything is already drawn", so
          ;; the copy has to stay off that path.
          (text nil))
      (dolist (f (reverse (latex-fragments)))
        (destructuring-bind (fbeg fend display) f
          (cond
            ;; Already drawn. Overlap and not containment: an overlay shifts
            ;; with the text around it, so a fragment whose source has grown by
            ;; a character is still the same equation.
            ((some (lambda (r) (and (< (car r) fend) (> (cdr r) fbeg))) have))
            ;; The one point is inside: you are still typing in it, and
            ;; `$\alph' is a LaTeX error, not an equation.
            ;;
            ;; Arm the walk flag on it, which is the half that was missing.
            ;; Skipping was never enough on its own — nothing was left saying
            ;; the fragment was owed a render, so a `$x^2$' the cursor happened
            ;; to be sitting in when this pass ran stayed spelled out for good:
            ;; walking out of it changed no line and left no revealed overlay
            ;; behind, so neither trigger in `org-latex-maybe-preview' ever
            ;; fired again. That is the case you meet the moment you open a
            ;; file with point in an equation, and the one you meet every time
            ;; you edit inside a `\begin{align}' and step down a line still
            ;; inside it. The range is exactly what `org-latex-reveal-at-point'
            ;; would have recorded had there been an image to take off.
            ((and (<= fbeg at) (<= at fend))
             (setf *org-latex-revealed* (cons fbeg fend)))
            (t
             (unless text (setf text (buffer-string)))
             (%org-latex-draw fbeg fend (%org-latex-scale text fbeg display))))))))
  nil)

(defun org-latex-note-change ()
  "Remember that this line now wants typesetting. On `after-change-hook', so it
must stay this cheap: one reader and one SETF, no scan."
  (when (derived-mode-p 'org-mode)
    (setf *org-latex-edited-line* (line-number)
          ;; ...and the walk flag goes, because its offsets have just moved. See
          ;; `*org-latex-revealed*'.
          *org-latex-revealed* nil)))

(defun org-latex-reveal-at-point ()
  "Take the preview off the fragment point has moved into, showing its source.

The other half of `org-latex-preview-new', and what makes editing an equation
possible at all. An overlay carrying an image *substitutes* for the characters
under it — `display.lisp' gives them no cell of their own — so a character typed
inside a preview lands in the buffer and never appears on screen, and the
equation reads as though the keystroke was dropped. Emacs opens a preview the
cursor walks into for this exact reason; this is that rule.

Inside **or against either edge**, and the one-character slack is
`%org-modern-openable''s: `overlays-in' does not count touching at a boundary as
overlapping, and the boundaries are where you type. It agrees with
`%org-latex-fragment-at-point', which counts both delimiters as inside for the
same reason — point on the closing `$' of `$x^2$' is in that equation to anyone
who just typed it.

Records *where* it was on the way past, and that is how the image comes back:
`org-latex-maybe-preview' re-typesets the moment point is outside that range
again, so a cursor that merely walked through an equation leaves it drawn behind
it. It used to mark the *line* dirty instead, and the difference is two
equations on one line — see `*org-latex-revealed*'."
  (let ((ovs (remove-if-not (lambda (o) (overlay-get (first o) :latex))
                            (overlays-in (max 0 (1- (point))) (1+ (point))))))
    (when ovs
      (mapc (lambda (o) (delete-overlay (first o))) ovs)
      ;; The range comes back from the same query the handles did — `overlays-in'
      ;; already answers (ID BEG END), so remembering where the equation was
      ;; costs no second round trip. Deleting the overlay does not move any
      ;; text (an image *substitutes* for characters, it does not replace them),
      ;; so these offsets stay true for as long as nobody types.
      (setf *org-latex-revealed*
            (cons (reduce #'min ovs :key #'second)
                  (reduce #'max ovs :key #'third))))))

(defun %org-latex-walked-off-p ()
  "True when point has left the fragment `org-latex-reveal-at-point' opened.

Two integer comparisons against a cons, so the cheap half of the hook stays
cheap: NIL when nothing was revealed, which is every keystroke in every buffer
that has no equations in it."
  (let ((r *org-latex-revealed*))
    (and r (let ((p (point))) (or (< p (car r)) (> p (cdr r)))))))

(defun org-latex-maybe-preview ()
  "Typeset the fragments on the line point has left, and un-typeset the one it
has arrived in.

On `point-moved-hook'. The render half is guarded by two integers, so navigating
a buffer nobody has edited costs one comparison per keystroke; the reveal half
costs one `overlays-in' on top of that, which is what `org-modern-appear' has
always spent on this same hook to do this same job for bold and links.

The reveal runs *after* the render, and the order is the whole of the
two-equation case: revealing first would mark the line point just arrived on as
the dirty one, and the equation point just *left* would then never be drawn."
  (when (or (and *org-latex-edited-line*
                 (/= *org-latex-edited-line* (line-number)))
            (%org-latex-walked-off-p))
    (setf *org-latex-edited-line* nil
          *org-latex-revealed* nil)
    (org-latex-preview-new))
  (when (derived-mode-p 'org-mode)
    (org-latex-reveal-at-point))
  nil)

(add-hook '*after-change-functions* 'org-latex-note-change)
(add-hook '*point-moved-functions* 'org-latex-maybe-preview)
