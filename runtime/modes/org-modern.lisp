;;;; org-modern and org-appear — org's markup *drawn* rather than typed.
;;;;
;;;; An overlay carrying a `display' string is drawn instead of the characters
;;;; it covers, and the renderer substitutes cells rather than painting over
;;;; them — so `***' really does become one bullet and the heading text really
;;;; does stay where it was. That is the whole mechanism. Everything below is
;;;; policy: which ranges, which glyph, and when to stop hiding.
;;;;
;;;; `org-latex-preview' in `init.lisp' is the model, and this is the same shape
;;;; one size up: a Lisp-side property (`:org-modern') marks an overlay as ours,
;;;; so a refresh replaces exactly its own overlays and leaves the LaTeX images,
;;;; an avy hint or anything else alone. `remove-overlays' is the blunt
;;;; instrument and is never used here.
;;;;
;;;; org-appear is the other half: an overlay hides markup, so *deleting its
;;;; display* reveals it, and the question "which one is the cursor in" is one
;;;; `overlays-at'. Re-hiding re-reads the text underneath and renders it again
;;;; rather than restoring a string saved when the overlay was made — which is
;;;; what keeps typing *inside* a revealed fragment from putting a stale glyph
;;;; back over it afterwards.
;;;;
;;;; ponytail — the scanner is here, in Lisp, and it should not be.
;;;; `crates/syntax/src/org.rs' already has `bullets()', which answers exactly
;;;; the heading/list question with the level and the marker range, and it is
;;;; *not reachable from the image*: there is no arm for it in `query.rs' and no
;;;; name for it in `QUERIES_FORM'. `latex-fragments' is the precedent — one
;;;; reader, answered outside `query.rs' because org's scanner is downstream of
;;;; core — and an `org-bullets' beside it would delete `%org-modern-line' and
;;;; half of `%org-modern-scan' below. Until then the classification is repeated
;;;; here and can drift from the highlighter's; the two agree today, and the
;;;; places they must agree are marked.
;;;;
;;;; What used to be the ceiling you felt — no cursor-movement hook, so markup
;;;; revealed itself as you typed and not as you navigated — is closed:
;;;; `point-moved-hook' is the editor's second report about a buffer, and
;;;; `org-modern-appear' hangs off it beside the change hook at the foot of this
;;;; file.
;;;;
;;;; The ceiling that remains is `org-modern-refresh': markup *typed since the
;;;; last refresh* has no overlay to reveal, because a full rescan per keystroke
;;;; is not affordable and the editor reports no change delta. `SPC m m'.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; What gets drawn instead of what
;;;
;;; Five tables, all `defparameter' so a config can set them and re-run
;;; `org-modern-refresh' without reloading anything.
;;;
;;; **Every glyph in them has to be in the editor's own font.** That reads as an
;;; obvious constraint and was not obeyed: this file was written against Emacs's
;;; org-modern, whose glyphs come from whatever font your Emacs falls back
;;; through, and this editor falls back through nothing at all. `FONT_CANDIDATES'
;;; in `crates/render' picks the first monospace face that exists and draws every
;;; character out of it; SFNSMono is first on every modern macOS, and SFNSMono has
;;; no `◉', no `✸', no `✿', no `❀', no `✜' and no `☐'. Six of the eleven glyphs
;;; below drew *nothing* — a blank where the bullet should be, on every heading
;;; of every org buffer — and it went unnoticed for as long as it did because a
;;; missing glyph looks like indentation.
;;;
;;; So the tables below are chosen from what SFNSMono and Menlo both have, which
;;; is the first two entries of that list and therefore every Mac. The bullets
;;; lost some of their variety and gained the property of existing.
;;;
;;; ponytail: the coverage is checked by hand, once, by whoever edits these — a
;;; config that sets its own bullets can still pick a glyph its font does not
;;; have and get a blank with nothing anywhere saying why. Ceiling: exactly the
;;; bug this note is about, happening again in somebody's config. The real fix is
;;; one crate out and fixes it for every glyph rather than for these eleven: a
;;; *fallback* in `draw_char', trying the next face in `FONT_CANDIDATES' for a
;;; character the current one does not carry, which is what every other text
;;; stack on the machine does and what would also rescue the `☐' that
;;; `math.lisp' draws its status pills out of.

(defparameter *org-modern-stars* '("●" "○" "■" "□" "▸" "‣")
  "One bullet per heading level, cycling past the end.

A level-N heading substitutes N-1 spaces and then its bullet for its N stars,
so the glyph steps right as the level deepens and the heading *text* does not
move at all — the substitution is the same width as what it replaces.

Filled and hollow alternating, and a shape change every two levels, so the depth
is readable at a glance rather than by counting. Every one of them is in both
SFNSMono and Menlo — see the note above, which is what this list used to fail.")

(defparameter *org-modern-heading-scale* '(1.5 1.25)
  "Type size per heading level, as a multiple of the body's.

One entry per level from the top; a level past the end of the list is body size,
which is what stops a deep outline from being a wall of large type. Setting this
to NIL turns the whole thing off and leaves headings the colour they were.

A *multiple*, not a point size: the body follows `font-size' and a heading
follows the body, so C-+ enlarges the document rather than closing the gap
between its parts. The renderer snaps a size to the nearest of the few it will
open a font face at — that snapping is what bounds its glyph cache — so 1.4 and
1.5 are the same face here on purpose. This list is a statement of intent, not a
measurement.")

(defparameter *org-modern-list-bullets* '((#\- . "•") (#\+ . "◦"))
  "One glyph per list marker. `1.' and `1)' are deliberately absent, for the
reason `crates/syntax' gives: a number is not one character and has no glyph to
become.")

(defparameter *org-modern-checkboxes*
  '((#\Space . "□") (#\X . "✓") (#\x . "✓") (#\- . "✗"))
  "One glyph per `[ ]' cookie, keyed by the character between the brackets.
Three cells become one, so the item text does shift left — which is what
org-modern does too, and what makes a checkbox read as one thing.

An empty box, a tick and a cross rather than the ballot boxes `☐ ☑ ☒' Emacs's
org-modern uses: those three are the clearer set and *none of them is in
SFNSMono*, so all three drew nothing. See the note at the head of this section.")

(defparameter *org-modern-emphasis*
  '((#\* . "bold") (#\/ . "italic") (#\_ . "italic")
    (#\= . "code") (#\~ . "code") (#\+ . "comment"))
  "Emphasis marker -> the face its body is drawn in.

The same mapping `marker_kind' in `crates/syntax/src/org.rs' makes, and it has
to be: the overlay covers the *whole* run, markers included, so the display
string inherits the highlight at the run's first character — which is the
markup face of the marker — unless the overlay claims a face of its own.")

;;; ---------------------------------------------------------------------------
;;; Reading a line of org
;;;
;;; Line-oriented, because org is: it decides what a line *is* from its first
;;; few characters, and only `#+begin_'/`#+end_' blocks carry state across
;;; lines. One pass, one line at a time, exactly as `org.rs' does it.

(defun %org-blank-p (c) (member c '(#\Space #\Tab #\Return)))

(defun %org-directive-p (line word)
  "True when LINE, after its indentation, starts with WORD, ignoring case."
  (let* ((n (length line))
         (i (or (position-if-not #'%org-blank-p line) n))
         (m (length word)))
    (and (<= (+ i m) n) (string-equal word line :start2 i :end2 (+ i m)))))

(defun %org-border-p (c)
  "Org's BORDER: an emphasis body may not begin or end with whitespace, nor
with a comma."
  (and (not (%org-blank-p c)) (char/= c #\,)))

(defparameter *org-emphasis-pre* '(#\- #\( #\' #\" #\{)
  "What may sit immediately before an emphasis opener, besides whitespace and
the start of the line.")

(defparameter *org-emphasis-post* '(#\- #\. #\, #\: #\! #\? #\; #\' #\" #\) #\} #\[)
  "What may sit immediately after an emphasis closer, besides whitespace and
the end of the line.")

(defun %org-emphasis-at (line i)
  "Index of the marker closing an emphasis run that opens at I, or NIL.

Org's rule is a triple — the character before the opener, the two borders of
the body, the character after the closer — and all three are needed. Without
the first, `http://x/a/b/' is italics; without the second, `2 * 3' is bold."
  (let ((n (length line)) (c (char line i)))
    (when (and (assoc c *org-modern-emphasis*)
               (or (zerop i)
                   (%org-blank-p (char line (1- i)))
                   (member (char line (1- i)) *org-emphasis-pre*))
               (< (1+ i) n)
               (char/= (char line (1+ i)) c)
               (%org-border-p (char line (1+ i))))
      (loop for j from (+ i 2) below n
            when (and (char= (char line j) c)
                      (%org-border-p (char line (1- j)))
                      (or (= (1+ j) n)
                          (%org-blank-p (char line (1+ j)))
                          (member (char line (1+ j)) *org-emphasis-post*)))
              do (return j)))))

(defun %org-link-at (line i)
  "Index just past the `]]' of a `[[target]]' or `[[target][text]]' at I, or NIL."
  (let ((n (length line)))
    (when (and (char= (char line i) #\[)
               (< (1+ i) n)
               (char= (char line (1+ i)) #\[))
      (let ((close (loop for j from (+ i 2) below (1- n)
                         when (and (char= (char line j) #\])
                                   (char= (char line (1+ j)) #\]))
                           do (return j))))
        ;; `[[]]' links to nothing and is left as the four characters it is.
        (when (and close (> close (+ i 2))) (+ close 2))))))

(defun %org-link-parts (text)
  "The (TARGET . LABEL) of the whole link TEXT `[[target]]' or
`[[target][label]]', or NIL when TEXT is not one.

LABEL is TARGET when the link has no description, which is org's own rule and is
what lets every caller below treat the two spellings alike. The one place the
distinction survives is the *drawn* one: a bare link shows its target because
that is all there is to show.

Split out of `%org-modern-render' rather than beside it. Three things now want
the two halves of a link — what to draw, where to jump, and whether it names a
figure — and a link parsed three ways is a link parsed differently three ways
the first time somebody handles `[[a][b][c]]'."
  (let ((n (length text)))
    (when (and (> n 4)
               (string= "[[" text :end2 2)
               (string= "]]" text :start2 (- n 2)))
      (let* ((body (subseq text 2 (- n 2)))
             (split (search "][" body)))
        (if split
            (cons (subseq body 0 split) (subseq body (+ split 2)))
            (cons body body))))))

(defun %org-link-scheme (target)
  "(SCHEME . REST) when TARGET is `scheme:rest', else NIL. SCHEME is downcased.

A scheme is a letter followed by letters, digits and `+ - .', which is the URI
rule and is here to keep a *title* from being read as one: the Contents section
of a curriculum is full of `[[id:unit-1][1. Vectors and Spaces]]', and the
description half of that link has a colon in it about as often as not."
  (let ((colon (position #\: target)))
    (when (and colon (plusp colon) (alpha-char-p (char target 0))
               (every (lambda (c)
                        (or (alphanumericp c) (member c '(#\+ #\- #\.))))
                      (subseq target 0 colon)))
      (cons (string-downcase (subseq target 0 colon)) (subseq target (1+ colon))))))

(defun %org-link-rest (target)
  "TARGET with its scheme stripped — `file:a/b.png' is `a/b.png', and a target
that names no scheme is itself."
  (let ((s (%org-link-scheme target)))
    (if s (cdr s) target)))

(defparameter *org-image-file-types* '("png" "svg" "svgz")
  "Extensions `org-inline-images' will draw, lower-cased.

Exactly what `zemacs-figure' decodes and no more, because a link this list
claims and the decoder then refuses is a message in the status line rather than
a link. SVG for a diagram, PNG for a plot; a photograph is not a figure. See the
ceiling at the head of `crates/figure/src/lib.rs'.")

(defun %org-image-target (target)
  "TARGET's path when it names a file that could be drawn as a figure, else NIL.

A classification and not a probe: it says nothing about whether the file is
there, only that a link of this shape is the image path's business rather than
the link-drawing path's. Both callers want that question answered before either
of them touches the disk.

`file:' or no scheme at all. `https://example.com/a.png' is deliberately not a
figure: nothing here fetches, and drawing a broken image for it would be worse
than showing the URL you can read."
  (let* ((s (%org-link-scheme target))
         (path (if s (cdr s) target)))
    (when (or (null s) (string= (car s) "file"))
      (let ((dot (position #\. path :from-end t)))
        (when (and dot
                   (member (string-downcase (subseq path (1+ dot)))
                           *org-image-file-types* :test #'string=))
          path)))))

(defun %org-checkbox-at (line from)
  "(BEG . END) of a `[ ]', `[X]' or `[-]' cookie at or just after FROM, or NIL."
  (let* ((n (length line))
         (i (or (position-if-not #'%org-blank-p line :start (min from n)) n)))
    (when (and (< (+ i 2) n)
               (char= (char line i) #\[)
               (char= (char line (+ i 2)) #\])
               (assoc (char line (1+ i)) *org-modern-checkboxes*))
      (cons i (+ i 3)))))

(defun %org-modern-inline (line from)
  "The links and emphasis runs in LINE at or after FROM, as (BEG END KIND).

Markup does not nest, so a hit skips its whole body — which is also what stops
the closing marker of one run from opening the next."
  (let ((n (length line)) (out nil) (i from))
    (loop while (< i n)
          do (let ((link (%org-link-at line i)))
               (cond (link (push (list i link :link) out) (setf i link))
                     (t (let ((close (%org-emphasis-at line i)))
                          (cond (close (push (list i (1+ close) :emphasis) out)
                                       (setf i (1+ close)))
                                (t (incf i))))))))
    (nreverse out)))

(defun %org-modern-line (line)
  "The substitutions LINE asks for, as (BEG END KIND) in offsets into LINE.

The three lines that are *not* prose — a `#+directive', a `# comment' and a
`|table|' row — are skipped whole, which is the same decision `org.rs' makes
about them and for the same reason: a rule like `|--+--|' would otherwise read
as a `+strike+' run.

A heading is skipped whole too, and that one is a rule about agreement rather
than about taste: the highlighter paints a heading line in one face, emphasis
included, so substituting inside one would be the only place two things claimed
the same cells."
  (let* ((n (length line))
         (indent (or (position-if-not #'%org-blank-p line) n))
         (head (and (< indent n) (char line indent)))
         (stars (and (zerop indent) (eql head #\*)
                     (let ((k (or (position-if-not (lambda (c) (char= c #\*)) line) n)))
                       (and (< k n) (char= (char line k) #\Space) k)))))
    (cond
      ((null head) nil)
      ;; Two ranges, not one: the stars become a bullet and carry the line's
      ;; type size, and the text after them carries the weight. See the
      ;; `:heading-line' arm of `%org-modern-render' for why they cannot be the
      ;; same overlay.
      (stars (list (list 0 stars :heading)
                   (list stars n :heading-line)))
      ((member head '(#\# #\|)) nil)
      ;; `- item' / `+ item', then its checkbox, then the prose after both.
      ((and (member head '(#\- #\+))
            (< (1+ indent) n)
            (%org-blank-p (char line (1+ indent))))
       (let* ((box (%org-checkbox-at line (+ indent 2)))
              (from (if box (cdr box) (+ indent 2))))
         (append (list (list indent (1+ indent) :list))
                 (when box (list (list (car box) (cdr box) :checkbox)))
                 (%org-modern-inline line from))))
      (t (%org-modern-inline line indent)))))

(defun %org-modern-scan (text)
  "Every substitution TEXT wants, as (BEG END KIND LITERAL): BEG and END in
*character* offsets — which is what `make-overlay' takes — and LITERAL the text
between them, which is what `%org-modern-render' turns into a glyph.

Two offset spaces, and both are needed. `buffer-string' hands out UTF-8 bytes,
every offset the editor takes is a character, and `%char-index' converts. It is
called per *line* rather than per hit, because it counts continuation bytes from
the start of whatever it is given — over the whole buffer that would be
quadratic, and over a line it is a few dozen characters."
  (let ((n (length text)) (out nil) (in-block nil) (bol 0) (cbol 0))
    (loop while (<= bol n)
          do (let* ((eol (or (position #\Newline text :start bol) n))
                    (line (subseq text bol eol)))
               ;; A `- ' inside `#+begin_src' is a diff line or a shell flag,
               ;; and a `*' in one is a glob. An unterminated block swallows the
               ;; rest of the file, which beats guessing.
               (cond ((%org-directive-p line "#+begin_") (setf in-block t))
                     ((%org-directive-p line "#+end_") (setf in-block nil))
                     (in-block)
                     (t (dolist (s (%org-modern-line line))
                          (destructuring-bind (a b kind) s
                            (push (list (+ cbol (%char-index line a))
                                        (+ cbol (%char-index line b))
                                        kind
                                        (subseq line a b))
                                  out)))))
               (setf bol (1+ eol)
                     cbol (+ cbol (%char-index line (length line)) 1))))
    (nreverse out)))

;;; ---------------------------------------------------------------------------
;;; What a range is drawn as
;;;
;;; One function, used twice: the scan calls it to make an overlay, and
;;; org-appear calls it to re-hide one. That is deliberate — it means a revealed
;;; fragment can be *edited* while it is revealed and still hide correctly
;;; afterwards, because nothing was cached.

(defun %org-modern-render (kind text)
  "The overlay properties the literal TEXT of a KIND substitution wants, as a
plist, or NIL when TEXT is no longer that kind of markup at all.

NIL is the useful answer: an overlay whose text was edited under it retires
itself instead of drawing a glyph over something that is not there any more.

A **plist** and not the (DISPLAY . FACE) pair this used to answer, and the
reason is the shape of the thing on the other end rather than taste. An overlay
is a bag of display properties — `display', `face', and whatever the renderer
grows next — and deciding which of them a piece of markup wants is this
function's entire job. Both callers below apply whatever they are handed without
knowing a single property name, so *adding* one is a line in the arm that wants
it and nothing anywhere else. Making a heading's type larger, for instance, is a
property on the heading arm and no change to the machinery at all.

No `face' wherever the syntax highlighter already has the colour right — a
display string is attributed to the first character it covers, so it inherits
that character's highlight for free. Only emphasis and links need to say
otherwise, because the character they start on is a markup marker."
  (let ((n (length text)))
    (case kind
      (:heading
       (when (and (plusp n) (every (lambda (c) (char= c #\*)) text))
         ;; `scale' is a *line* property wherever it lands, so this overlay —
         ;; which covers only the stars — sets the type size of the whole
         ;; heading line. Nothing in the renderer knows the word "heading";
         ;; this list is the entire policy.
         (let ((scale (nth (1- n) *org-modern-heading-scale*)))
           (append (list 'display
                         (concatenate 'string
                                      (make-string (1- n) :initial-element #\Space)
                                      (nth (mod (1- n) (length *org-modern-stars*))
                                           *org-modern-stars*)))
                   ;; `scale' is line-wide, so setting it on the stars sizes the
                   ;; whole heading. Weight is *not* — see `:heading-line'.
                   (when scale (list 'scale scale))))))
      ;; The heading's text, claiming weight and nothing else.
      ;;
      ;; A separate range because the two properties have different reach:
      ;; `scale' belongs to the line (a taller cell is a fact about the row),
      ;; while `weight' belongs to the run, since it changes no metric. Setting
      ;; `weight' on the stars above would therefore have emboldened exactly one
      ;; character — the bullet glyph substituted for them — and left the words
      ;; light, which is the opposite of the intent.
      ;;
      ;; Every level gets it, including the ones past
      ;; `*org-modern-heading-scale*' that are body-sized: what says "heading" in
      ;; a typeset document is the weight, and a deep heading at body size with
      ;; body weight is just a sentence.
      (:heading-line
       (when (plusp n) (list 'weight 'bold)))
      (:list
       (let ((glyph (and (= n 1) (cdr (assoc (char text 0) *org-modern-list-bullets*)))))
         (when glyph (list 'display glyph))))
      (:checkbox
       (let ((glyph (and (= n 3)
                         (char= (char text 0) #\[)
                         (char= (char text 2) #\])
                         (cdr (assoc (char text 1) *org-modern-checkboxes*)))))
         (when glyph (list 'display glyph))))
      (:emphasis
       (let ((face (and (> n 2) (cdr (assoc (char text 0) *org-modern-emphasis*)))))
         (when (and face (char= (char text 0) (char text (1- n))))
           (append
            (list 'display (subseq text 1 (1- n)) 'face face)
            ;; ...and now really bold and really italic, rather than the colour
            ;; the face table had to stand in for while the renderer opened one
            ;; font. Read off the face name rather than from a second table: the
            ;; two would be one mapping written twice and would drift.
            ;;
            ;; The face stays either way — it is what carries `code' and
            ;; `comment', and a theme may well want bold text tinted as well as
            ;; heavy.
            (cond ((string= face "bold") (list 'weight 'bold))
                  ((string= face "italic") (list 'slant 'italic)))))))
      (:link
       (let ((parts (%org-link-parts text)))
         ;; ...and NIL for a link that names a figure, which is the one kind of
         ;; markup this file deliberately does not draw. `org-inline-images'
         ;; puts a *bitmap* over exactly those cells, and two overlays claiming
         ;; the same range is the one case the renderer resolves arbitrarily —
         ;; the older one wins its substitution and the newer one is dropped, so
         ;; whichever of the two passes ran first would decide whether you saw
         ;; the figure or the words `[[file:fig-1.svg]]'. Declining here is what
         ;; makes that question not exist.
         (when (and parts (not (%org-image-target (car parts))))
           (list 'display (cdr parts) 'face "link")))))))

(defun %org-modern-apply (ov look)
  "Put every property in the plist LOOK on overlay OV.

The one place a property name is *not* written out, which is what keeps
`%org-modern-render' the only file that has to know them."
  (loop for (prop value) on look by #'cddr
        do (overlay-put ov prop value)))

;;; ---------------------------------------------------------------------------
;;; The overlays
;;;
;;; `:org-modern' holds the *kind*, which is both the mark saying "this one is
;;; mine" and everything `%org-modern-render' needs to draw it again. One
;;; property, two jobs.

(defun %org-modern-overlays (&optional (beg (point-min)) (end (point-max)))
  "Handles of the overlays this file made, overlapping BEG..END."
  (remove-if-not (lambda (o) (overlay-get o :org-modern))
                 (mapcar #'first (overlays-in beg end))))

(defvar *org-modern-revealed* nil
  "The one overlay currently showing its literal markup, or NIL.

DEFVAR rather than DEFPARAMETER: reloading the config must not forget that
something on screen is revealed, or the next `org-modern-appear' would leave it
revealed forever.")

(defvar *org-modern-appear-inhibit-modes* nil
  "Major modes in which markup is never revealed under the cursor.

*The policy hook for reveal-on-cursor*, and the one thing about this file a
**renderer** has to switch off. `org-appear' exists because the buffer is being
edited: the asterisks around a word have to come back when you move onto them,
or you cannot see what you are about to type into. A mode where nothing is typed
has no use for that and is actively harmed by it — markup reappearing because
the cursor drifted is exactly the machinery such a mode promised was not on the
page.

A list of mode names rather than a test for one, and pushed onto from the mode
that wants it: `org-frozen.lisp' adds itself and this file does not know it
exists. The same shape `*org-mode-functions*', `*after-change-functions*' and
`*fold-subtree-functions*' have, and for the same reason — a second mode wanting
it is one PUSHNEW and no edit here.

Asked with `derived-mode-p', so naming a parent inhibits every mode under it.")

(defun %org-modern-remove ()
  "Drop every overlay this file made, and answer how many there were.

Exactly its own: `remove-overlays' would take the LaTeX previews with them,
which is what the `:org-modern' mark exists to prevent."
  (setf *org-modern-revealed* nil)
  (let ((ovs (%org-modern-overlays)))
    (mapc #'delete-overlay ovs)
    (length ovs)))

(defun org-modern-clear ()
  "Take every substitution off, showing org's punctuation again."
  (message (format nil "~d substitution~:p cleared" (%org-modern-remove))))

(defun org-modern-refresh ()
  "Redraw every substitution in the buffer.

Its own overlays first, so this doubles as `refresh' and is safe to call as
often as you like. One scan of `buffer-string' and one overlay per hit — which
is why it is a *command* and not something on `after-change-hook' — see the
note beside that hook at the foot of this file."
  (%org-modern-remove)
  (let ((made 0))
    (dolist (s (%org-modern-scan (buffer-string)))
      (destructuring-bind (beg end kind text) s
        (let ((look (%org-modern-render kind text)))
          (when look
            (let ((ov (make-overlay beg end)))
              (when ov
                (overlay-put ov :org-modern kind)
                (%org-modern-apply ov look)
                (incf made)))))))
    ;; Whatever the cursor is already sitting in should not have been hidden.
    (org-modern-appear)
    ;; Ends on the message, so it answers NIL: `eval-string' echoes the value of
    ;; the last form and would otherwise wipe out what this just said.
    (message (format nil "~d substitution~:p" made))))

(defun %org-modern-rehide (ov)
  "Put OV's glyph back, re-reading the text it covers."
  (when ov
    (let* ((at (overlay-position ov))
           (look (and at (%org-modern-render (overlay-get ov :org-modern)
                                             (buffer-substring (car at) (cdr at))))))
      (cond
        ;; NIL from `overlay-position' means gone *or* in another buffer, and
        ;; nothing tells the two apart — so this forgets it rather than deleting
        ;; it. Left revealed in a buffer you are not looking at, an overlay
        ;; shows the markup it covers, which is what the mode being off looks
        ;; like anyway; deleting somebody else's buffer's overlay is not.
        ((null at))
        (look (%org-modern-apply ov look))
        ;; The text under it is not the markup it was made for any more — an
        ;; overlay that cannot draw itself has nothing to say.
        (t (delete-overlay ov))))))

(defun org-modern-appear ()
  "Reveal the literal markup the cursor is inside, and hide everything else.

org-appear, in a dozen lines and no state worth the name: at most one overlay
is revealed at a time, so the work is `is the cursor still in the one I
revealed' and, when it is not, two `overlay-put's. That is what makes this
affordable on every keystroke where a full rescan is not."
  (when (and (minor-mode-p 'org-modern)
             (notany #'derived-mode-p *org-modern-appear-inhibit-modes*))
    (let ((now (first (%org-modern-overlays (point) (1+ (point))))))
      (unless (eql now *org-modern-revealed*)
        (%org-modern-rehide *org-modern-revealed*)
        ;; NIL clears the property, so the characters underneath are drawn —
        ;; which is the whole of "reveal".
        (when now (overlay-put now 'display nil))
        (setf *org-modern-revealed* now))))
  ;; Answers NIL rather than an overlay handle: this is a command as well as a
  ;; hook, and `M-x' echoing an integer at you says nothing.
  nil)

;;; ---------------------------------------------------------------------------
;;; Reading the buffer a line at a time
;;;
;;; One query and a walk, rather than `line-string' per line. Everything below
;;; this point — following a link, finding an `:ID:', deciding which links are
;;; figures — is a question about the whole document, and asking the editor once
;;; is what keeps that from being a thousand round trips through `%query' on a
;;; file the size of a textbook.
;;;
;;; `%org-modern-scan' has this loop inlined and predates it; the two agree
;;; because they are the same six lines, and the day the scanner is rewritten
;;; against `crates/syntax' (see the ponytail note at the top of this file) it
;;; goes away and this one stays.

(defun %org-lines (&optional (text (buffer-string)))
  "Every line of TEXT as (STRING BEGIN END).

BEGIN and END are *character* offsets, which is what every editor primitive
takes; TEXT arrives as UTF-8 bytes, which is what `%char-index' is for. END is
the newline's own offset — so `(subseq buffer BEGIN END)' is the line without
it, and END is also where a line-final insertion goes.

Converted per *line* and not per hit, for `%org-modern-scan''s reason: the
conversion counts continuation bytes from the start of whatever it is handed,
which over a whole buffer would be quadratic and over one line is a few dozen
characters."
  (let ((n (length text)) (out nil) (bol 0) (cbol 0))
    (loop while (<= bol n)
          do (let* ((eol (or (position #\Newline text :start bol) n))
                    (line (subseq text bol eol))
                    (chars (%char-index line (length line))))
               (push (list line cbol (+ cbol chars)) out)
               (setf bol (1+ eol) cbol (+ cbol chars 1))))
    (nreverse out)))

(defun %org-line-level (line)
  "LINE's headline level — its run of leading `*' — or NIL when it is not one.

The string-taking twin of `%org-level' in `org-fold.lisp', which asks the editor
for a line by number. Both apply org's own rule, which is what makes `**bold**'
at the start of a line not a headline: stars at column 0 *followed by a space*."
  (let ((n (or (position-if-not (lambda (c) (char= c #\*)) line) (length line))))
    (when (and (plusp n) (< n (length line)) (char= (char line n) #\Space))
      n)))

(defun %org-property (line name)
  "The value of the property drawer line LINE when it names NAME, else NIL.

`:ID: unit-1' with NAME `\"ID\"' is `\"unit-1\"'. Case-insensitive in the name,
as org is, and the value is trimmed — which is the whole reason this is a
function and not a `search': a drawer line is indented, org writes `:ID:  x'
about as often as `:ID: x', and every reader of a property would otherwise strip
its own whitespace slightly differently.

An empty value answers the empty string rather than NIL, so `:ZEMACS_STATUS:'
with nothing after it is distinguishable from no such property at all."
  (let* ((s (string-trim '(#\Space #\Tab #\Return) line))
         (n (length name)))
    (when (and (> (length s) (1+ n))
               (char= (char s 0) #\:)
               (string-equal name s :start2 1 :end2 (1+ n))
               (char= (char s (1+ n)) #\:))
      (string-trim '(#\Space #\Tab) (subseq s (+ n 2))))))

;;; ---------------------------------------------------------------------------
;;; Following a link
;;;
;;; `RET' on a link, which is the binding the user's Emacs config has had in org
;;; normal state for years and which this editor simply did not have. It is
;;; cheap because the parser was already here: `%org-link-at' finds the extent,
;;; `%org-link-parts' splits it, and the rest of this section is *policy* — one
;;; table saying what a scheme means.
;;;
;;; The table is the point. `id:' and `file:' are what a curriculum needs, `http'
;;; is what a citation needs, and a config that wants `doi:' or `roam:' adds a
;;; line and a function with nothing to rebuild. That is the same shape
;;; `*fold-subtree-functions*' has in `org-fold.lisp', for the same reason.

(defparameter *org-link-open-functions*
  '(("id"     . org-link-open-id)
    ("file"   . org-link-open-file)
    ("http"   . org-link-open-url)
    ("https"  . org-link-open-url)
    ("mailto" . org-link-open-url))
  "Link scheme -> the function that follows it. *This is the policy hook.*

Called with the **whole** target, scheme and all, and not with the part after
the colon: `http' needs its scheme back to be a URL again, and one argument that
never has to be reassembled beats two that have to agree. `%org-link-rest' is
there for the handlers that want the other half.

A scheme with no entry, and a target with no scheme at all, fall through to
`%org-link-open-plain' below.")

(defparameter *org-link-opener*
  #+darwin "open" #+(or linux freebsd) "xdg-open" #-(or darwin linux freebsd) nil
  "The program handed a URL this editor cannot open itself, or NIL to refuse.

Not a browser name: the point of `open' and `xdg-open' is that the *desktop*
decides, so a `mailto:' reaches your mail client and an `https:' reaches the
browser you actually use.")

(defun %org-link-at-point ()
  "The (TARGET . LABEL) of the link point is inside, or NIL.

Point's column has to be converted into the same index space `%org-link-at'
reports in, which is bytes — `line-string' hands out UTF-8 like `buffer-string'
does, and a heading with an accent in it would otherwise put the cursor one
character to the left of where it is."
  (let* ((line (line-string))
         (col (%byte-index line (- (point) (line-start)))))
    (loop for (beg end kind) in (%org-modern-inline line 0)
          when (and (eq kind :link) (<= beg col) (< col end))
            return (%org-link-parts (subseq line beg end)))))

(defun %org-expand-file (name)
  "NAME as a path, resolved the way a link in a document means it.

Relative to the *file's own directory* and not to the process's, which is what
makes `[[file:fig-1.svg]]' mean the figure next to the .org file rather than one
next to wherever the editor was launched from. `~/' is expanded here because CL
does not do it, and a buffer with no file behind it leaves NAME alone — there is
nothing to be relative to."
  (let ((base (buffer-file-name)))
    (cond ((and (> (length name) 1) (string= "~/" name :end2 2))
           (namestring (merge-pathnames (subseq name 2) (user-homedir-pathname))))
          ((null base) name)
          (t (namestring
              (merge-pathnames name
                               (make-pathname :name nil :type nil
                                              :defaults (parse-namestring base))))))))

(defun %org-id-heading (id &optional (lines (%org-lines)))
  "Character offset of the heading whose `:ID:' is ID, or NIL.

The *heading*, not the property line: a drawer belongs to the headline above it,
and landing on `:ID: unit-1' would put point inside a drawer — which `org-fold'
may have closed, and which is not where you meant to go anyway.

`:CUSTOM_ID:' counts too. Org keeps them apart because one is a UUID it
generates and the other is a name you chose; nothing here generates anything, so
a link that finds either has found what it was looking for."
  (let ((heading nil))
    (dolist (l lines)
      (destructuring-bind (text begin end) l
        (declare (ignore end))
        (cond ((%org-line-level text) (setf heading begin))
              ((let ((v (or (%org-property text "ID")
                            (%org-property text "CUSTOM_ID"))))
                 (and v (string= v id)))
               (return (or heading begin))))))))

(defun %org-heading-position (title &optional (lines (%org-lines)))
  "Character offset of the first heading whose text is TITLE, or NIL.
Org's `[[*Heading]]', and the fallback for a bare target that matches one."
  (dolist (l lines)
    (destructuring-bind (text begin end) l
      (declare (ignore end))
      (let ((level (%org-line-level text)))
        (when (and level
                   (string-equal (string-trim '(#\Space #\Tab #\Return)
                                              (subseq text level))
                                 (string-trim '(#\Space #\Tab) title)))
          (return begin))))))

(defun org-link-open-id (target)
  "Follow `id:...' — jump to the heading carrying that `:ID:'."
  (let* ((id (%org-link-rest target))
         (at (%org-id-heading id)))
    (cond (at (goto-char at) (message (format nil "id: ~a" id)))
          (t (message (format nil "no heading with :ID: ~a" id))))))

(defun org-link-open-file (target)
  "Follow `file:...' — open the file, relative to this one's directory.

`find-file' reaches the *application* rather than the editor core, so the new
buffer arrives a frame later and nothing here can act on it. That is also why
the file is probed first: an open that will fail should say so now, in a message
naming the path, rather than a frame later in whatever the app decides to say."
  (let ((path (%org-expand-file (%org-link-rest target))))
    (cond ((probe-file path) (find-file path))
          (t (message (format nil "no such file: ~a" path))))))

(defun org-link-open-url (target)
  "Hand TARGET to the desktop — a browser for `http', a mail client for
`mailto'.

`:wait nil' because nothing here wants the browser's exit status and waiting for
one would park the Lisp thread on a program the user is still reading. Wrapped
in IGNORE-ERRORS because a machine with no opener is a message, not a backtrace,
and the message is worth having: the URL is printed either way, so a refusal
still leaves you something to copy."
  (let ((opener *org-link-opener*))
    (cond ((null opener) (message (format nil "no opener for ~a" target)))
          (t (ignore-errors
              (ext:run-program opener (list target)
                               :wait nil :input nil :output nil :error nil))
             (message (format nil "opened ~a" target))))))

(defun %org-link-open-plain (target)
  "Follow a target with no scheme — org's own fallbacks, in org's own order.

`*Heading' is a headline. `#name' is a `:CUSTOM_ID:'. Anything else is a file if
one is there, and otherwise a search: org's plain link is a *fuzzy* one, and
`[[Vectors and Spaces]]' meaning the heading of that name is the reason a
Contents section can be written without ids at all."
  (let ((n (length target)))
    (cond
      ((zerop n) (message "empty link"))
      ((char= (char target 0) #\*)
       (let ((at (%org-heading-position (subseq target 1))))
         (if at (goto-char at) (message (format nil "no heading: ~a" (subseq target 1))))))
      ((char= (char target 0) #\#) (org-link-open-id (subseq target 1)))
      ((probe-file (%org-expand-file target)) (find-file (%org-expand-file target)))
      (t (let ((at (or (%org-heading-position target) (search-forward target 0))))
           (if at (goto-char at) (message (format nil "not found: ~a" target))))))))

(defun org-open-at-point ()
  "Follow the link under point.

`RET' in an org buffer, which is what the table above makes worth having: a
Contents section of `[[id:unit-1][1. Vectors and Spaces]]' becomes a document you
navigate with one key rather than a list you read and then go hunting through."
  (let ((link (%org-link-at-point)))
    (if (null link)
        (message "no link at point")
        (let* ((target (car link))
               (scheme (car (%org-link-scheme target)))
               (open (and scheme (cdr (assoc scheme *org-link-open-functions*
                                             :test #'string=)))))
          (if (and open (fboundp open))
              (funcall open target)
              (%org-link-open-plain target))))))

;;; ---------------------------------------------------------------------------
;;; Figures
;;;
;;; `[[file:fig-1.svg]]' alone on a line, drawn as the figure. The mechanism is
;;; `org-latex-preview''s, whole: an overlay carrying `image', an id from a
;;; primitive that produced pixels, and the renderer growing the line to fit the
;;; bitmap. Nothing here is new machinery — the *only* thing that had to be built
;;; in Rust was a second producer of an id, because the first one could only
;;; render LaTeX. See `image-file' and `crates/figure'.
;;;
;;; Alone on its line is the whole of the rule, and it is org's: a link with
;;; prose around it is a reference and stays words, a link on its own line is a
;;; figure. That is also what makes the substitution safe — an image reserves
;;; cells but nothing to its right reflows (see `draw_image'), so the only place
;;; a wide one can paint over is a line with nothing else on it.
;;;
;;; ponytail: no `#+CAPTION:' and no `#+ATTR' width. A caption is a line of prose
;;; above the figure and already reads as one; a per-figure width is a keyword to
;;; parse and a second argument to thread, for a document whose figures all want
;;; the same measure. `*org-image-width*' is that measure, and the day one figure
;;; genuinely differs it becomes the default rather than the only answer.
;;;
;;; ponytail: the cursor does not reveal a figure the way it reveals emphasis, so
;;; editing the path under one means `SPC m F' first. org-appear's state is one
;;; overlay keyed by `:org-modern' and a figure is not one of those; joining in
;;; would mean the reveal machinery growing a second notion of what it owns.

(defparameter *org-image-width* 34
  "Widest a figure is drawn, in ems — NIL for the size it was authored at.

Ems and not pixels, and that is not a stylistic choice: the *device* pixel is
something only the renderer knows, so a width computed in the image from
`(font-size)' would come out half-size on a Retina display and full-size beside
it. `image-file' resolves this against the em the renderer parked on the editor,
which is the same correction `org-latex-preview' makes with its dpi.

A **maximum**. Nothing is scaled up, so a small diagram stays small and a plot
exported at 1600 px is brought down to something a pane can hold. 34 ems is
roughly a 55-column measure, which is a figure you can read with text beside
it.")

(defun %org-images (&optional (beg (point-min)) (end (point-max)))
  "Handles of the figure overlays this file made, overlapping BEG..END.

`:org-image' is the mark, exactly as `:org-modern' and `:latex' are theirs, so
clearing figures leaves equations and bullets alone. It holds the *path*, which
makes it the answer to `which file is this' as well as the mark — one property,
two jobs, which is the shape `:org-modern' already had."
  (remove-if-not (lambda (o) (overlay-get o :org-image))
                 (mapcar #'first (overlays-in beg end))))

(defun %org-image-links (&optional (lines (%org-lines)))
  "Every figure link alone on its line, as (BEGIN END PATH) in character offsets.

BEGIN and END cover the link *text* and not the whole line, so the overlay
replaces `[[file:fig-1.svg]]' and leaves the line's indentation where it was —
which is what indents a figure inside a list item along with the item."
  (let ((out nil))
    (dolist (l lines (nreverse out))
      (destructuring-bind (text begin end) l
        (declare (ignore end))
        (let ((i (position-if-not #'%org-blank-p text)))
          (when i
            (let ((j (%org-link-at text i)))
              (when (and j (every #'%org-blank-p (subseq text j)))
                (let* ((parts (%org-link-parts (subseq text i j)))
                       (path (and parts (%org-image-target (car parts)))))
                  (when path
                    (push (list (+ begin (%char-index text i))
                                (+ begin (%char-index text j))
                                path)
                          out)))))))))))

(defun %org-image-draw (beg end path)
  "Draw PATH over BEG..END. T when there were pixels, NIL when there were not —
which is the answer callers branch on, the same way `%org-latex-draw' works.

Two calls and not one, and they cannot be merged: `image-file' has to read and
rasterise before there is an id to hang, and it is the slow half. It runs on the
Lisp thread, so the editor keeps drawing and keeps taking keystrokes while a
buffer full of plots is decoded."
  (let ((id (image-file (%org-expand-file path) *org-image-width*)))
    (when id
      (let ((ov (make-overlay beg end)))
        (when ov
          (overlay-put ov :org-image path)
          (overlay-put ov 'image id)))
      t)))

(defun org-inline-images-clear ()
  "Take the figures off, showing their links again."
  (let ((ovs (%org-images)))
    (mapc #'delete-overlay ovs)
    (message (format nil "~d figure~:p cleared" (length ovs)))))

(defun org-inline-images ()
  "Draw every figure in the buffer.

Its own overlays first, so this doubles as `refresh' and is safe to press as
often as you like — which is how you see a figure you have just regenerated: the
id `image-file' answers with is derived from the file's length and mtime, so a
rewritten .svg is genuinely re-read rather than served from the id it had
before."
  (mapc #'delete-overlay (%org-images))
  (let ((drawn 0) (failed 0))
    (dolist (f (%org-image-links))
      (destructuring-bind (beg end path) f
        (if (%org-image-draw beg end path) (incf drawn) (incf failed))))
    (message (if (plusp failed)
                 (format nil "~d figure~:p, ~d could not be read" drawn failed)
                 (format nil "~d figure~:p" drawn)))))

(defun org-inline-images-new ()
  "Draw the figures that have none yet, quietly.

The hook version, and the same two-query shape `org-latex-preview-new' has: ask
once for the links and once for the overlays, and read a file only for what is
genuinely new. A buffer whose figures are all drawn costs those two queries and
no disk at all.

Quiet on success and quiet on failure both: a missing figure has already put
`image: cannot read ...' on the status line from inside the primitive, and
saying it twice per redraw would be the editor talking over you."
  (when (derived-mode-p 'org-mode)
    (let ((have (let ((out nil))
                  (dolist (o (overlays-in (point-min) (point-max)) out)
                    (when (overlay-get (first o) :org-image)
                      (push (cons (second o) (third o)) out))))))
      (dolist (f (%org-image-links))
        (destructuring-bind (beg end path) f
          ;; Overlap and not containment, for `org-latex-preview-new''s reason:
          ;; an overlay shifts with the text around it, so a link whose path has
          ;; grown by a character is still the same figure.
          (unless (some (lambda (r) (and (< (car r) end) (> (cdr r) beg))) have)
            (%org-image-draw beg end path))))))
  nil)

;;; ---------------------------------------------------------------------------
;;; The minor mode, and the one hook there is

(define-minor-mode org-modern
  "Draw org's markup instead of its punctuation: heading stars become bullets,
`- ' becomes `•', `[X]' becomes `✓', `[[a][b]]' becomes `b', and `*bold*'
becomes bold text with its asterisks hidden until the cursor is inside it.

Turning it off removes exactly the overlays it made."
  (:on (org-modern-refresh))
  (:off (%org-modern-remove)))

;;; `after-change-hook' is defined in `lsp.lisp', which `init.lisp' loads first
;;; and inside a `handler-case' — so a machine with no `rpc.lisp' would leave
;;; this file with nowhere to hang. Both forms are no-ops when that file did
;;; load: DEFVAR does not overwrite a bound variable, and the function is only
;;; defined when there is not one already.
;;;
;;; ponytail: this hook, and `*after-change-functions*' with it, belongs in the
;;; standard library beside `define-derived-mode'. It is in the LSP client
;;; because the LSP client was the only thing that wanted it, and this file is
;;; the second — which is the day it should move.
(defvar *after-change-functions* nil
  "Functions called with no arguments after any change to the live buffer.")

(unless (fboundp 'after-change-hook)
  (defun after-change-hook ()
    (dolist (f *after-change-functions*) (ignore-errors (funcall f)))))

;;; Hanging org-appear off both events the editor reports about a buffer.
;;;
;;; The *cursor* one is what makes this feel live, and it used to be the ceiling
;;; this file complained about at length: `after-change-hook' fires when the
;;; document moves, not when the cursor does, so in Normal mode `j' and `w'
;;; changed nothing, nothing fired, and the asterisks around the word you had
;;; just moved onto only appeared once you pressed `i' and typed a character.
;;; `point-moved-hook' — queued by the application from the same place, taking
;;; the same route through `pending_hooks' and the same `fboundp' guard, and
;;; declared in `modes.lisp' — closes it, and closing it cost exactly the second
;;; PUSHNEW below.
;;;
;;; Rebinding the motion keys to Lisp wrappers was the other way, and it was the
;;; wrong one: it would have reimplemented counts, operators and the desired
;;; column in the image to buy a hook, which is the trade the boundary exists to
;;; refuse.
;;;
;;; Both, and not just the motion one: a change that leaves point where it was —
;;; `x' on the last character of a line, an edit arriving from Lisp — still
;;; changes what point is *inside*. `org-modern-appear' is one comparison when
;;; nothing has moved, so the overlap costs a function call rather than a scan,
;;; and `after-change-hook' is one Lisp evaluation whether this is on it or not.
;;;
;;; Note what is *not* hung here: `org-modern-refresh'. A rescan is one query
;;; plus an overlay per hit, and doing that per keystroke would put a `%do' per
;;; bullet between you and your next character. So markup typed since the last
;;; refresh has no glyph until you ask for one — `SPC m m', or leaving and
;;; re-entering the mode. Closing that properly wants the change *delta*
;;; `boundary.org' lists as missing, which would let this rescan the two lines
;;; that moved instead of all of them.
(pushnew 'org-modern-appear *after-change-functions*)

;;; DEFVAR before the PUSHNEW for the same reason as above: `modes.lisp' is
;;; where this list is declared, and a build whose `*runtime-dir*' never found
;;; that file must get an empty list here rather than an unbound variable.
(defvar *point-moved-functions* nil)
(pushnew 'org-modern-appear *point-moved-functions*)

;;; ---------------------------------------------------------------------------
;;; Keys, and turning it on
;;;
;;; `org-mode' is declared in `library.lisp' with an empty body; this re-declares
;;; it with one, which is how the mode system is meant to be bent — the tables
;;; `define-derived-mode' writes are `*mode-parents*' and `*mode-bodies*', and
;;; the `set-mode-local' claim on `relative-line-numbers' lives in a third table
;;; and survives untouched. Re-declaring rather than wrapping is what makes a
;;; config reload idempotent instead of stacking a wrapper per reload.
;;;
;;; Guarded by `minor-mode-p' because the body runs on every entry into the
;;; mode, and `org-modern' is a toggle.

;;; `org-latex-preview-new' comes from `init.lisp', which is read before this
;;; file — so the FBOUNDP is about a *config* that never loaded it rather than
;;; about load order. It is here and not there because this is the last
;;; `define-derived-mode org-mode' in the runtime and therefore the only body
;;; that runs: declaring one in `init.lisp' would be silently replaced by this.
;;;
;;; Entering the mode is the right moment for it. The buffer has just been read,
;;; nothing is being typed into it, and previewing here is what makes an org
;;; file *arrive* typeset instead of arriving as `$\int_0^1 x^2 dx$' and waiting
;;; for you to ask.

;;; ...and figures for the same reason and at the same moment: a document
;;; arrives with its plots in it rather than with the paths to its plots in it.
;;; Cheaper than the LaTeX pass — a file read and a rasterise, no subprocess —
;;; and it is `-new', so re-entering the mode redraws nothing that is already
;;; drawn.

;;; ...and one hook list, because this body is the *only* one that runs.
;;;
;;; `define-derived-mode' writes into `*mode-bodies*', so the last declaration of
;;; `org-mode' in the runtime replaces every earlier one — which is what the
;;; comment above is about, and which means a file loaded after this one cannot
;;; add to org's entry without silently dropping everything in it. A hook list is
;;; the way in: PUSHNEW onto it, and a config reload does not stack a second copy
;;; the way wrapping the body would. `*after-change-functions*' and
;;; `*point-moved-functions*' are the same shape for the same reason, and
;;; `math.lisp' is the first customer.
;;;
;;; IGNORE-ERRORS per function, as those two do: one config's broken hook must
;;; not stop the next one from running, and must not turn opening a `.org' file
;;; into a backtrace.

(defvar *org-mode-functions* nil
  "Functions called with no arguments on entry into `org-mode'.")

(define-derived-mode org-mode text-mode
  (unless (minor-mode-p 'org-modern) (org-modern))
  (when (fboundp 'org-latex-preview-new) (org-latex-preview-new))
  (org-inline-images-new)
  (dolist (f *org-mode-functions*) (ignore-errors (funcall f))))

(define-key "org-mode" "SPC m m" "org-modern-refresh")
(define-key "org-mode" "SPC m M" "org-modern")
(define-key "org-mode" "SPC m a" "org-modern-appear")
(define-key "org-mode" "SPC m f" "org-inline-images")
(define-key "org-mode" "SPC m F" "org-inline-images-clear")

;;; `RET' follows a link, which is the binding an Emacs config has bound in org
;;; normal state for as long as there has been one.
;;;
;;; Safe in exactly the way it has to be: a *mode* keymap is consulted from
;;; `normal_key' and nowhere else, so this claims `RET' in org buffers in Normal
;;; and Visual state and leaves Insert alone — pressing return while typing
;;; still types a return. Binding it globally would not have that property, and
;;; is the reason it is spelled `"org-mode"' rather than `"normal"'.
;;;
;;; And it displaces nothing: `<ret>' has no arm in the built-in Normal grammar,
;;; so before this it did nothing at all. vim's `+' — first non-blank of the next
;;; line — was never here to lose.
;;;
;;; `SPC m o' beside it, because RET is the key you *use* and a leader binding
;;; is the one which-key can tell you about.
(define-key "org-mode" "<ret>" "org-open-at-point")
(define-key "org-mode" "SPC m o" "org-open-at-point")
