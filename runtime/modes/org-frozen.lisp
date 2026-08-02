;;;; org-frozen — org as a printed page rather than as a manuscript.
;;;;
;;;; `org-modern.lisp' draws org's *inline* markup: stars become bullets,
;;;; `[[a][b]]' becomes `b', `*bold*' becomes bold. It stops there on purpose,
;;;; because it has to: the cursor is in that buffer and something is being
;;;; typed into it, so every glyph it hides has to come back when you move onto
;;;; it, and nothing may hide a line you might want to edit.
;;;;
;;;; This file is what org looks like when that constraint is *lifted*. Nothing
;;;; is typed here — the buffer is genuinely read-only, enforced in core — and
;;;; every trade-off that normally protects editing is spent on looking right
;;;; instead.
;;;;
;;;; ** It is a scene, and that is the whole of the change
;;;;
;;;; This mode used to draw the page with *overlays on the cell grid*: a fold
;;;; for a drawer, a `display' string per table row, a `line-background' for a
;;;; listing, a `scale' on a heading's whole line. It works, and about a third
;;;; of the file was machinery whose only job was making a grid pretend to be a
;;;; page — five functions to hide a run of lines, one to repeat `─' into a
;;;; rule, one to pad table cells with spaces, one to take an escaping comma off
;;;; two characters because a `display' of the empty string clears the property
;;;; rather than hiding anything.
;;;;
;;;; All of that is gone. The page is a **scene** now — `docs/gui.org' is the
;;;; argument and `runtime/gui.lisp' is the API — a tree of boxes and paragraphs
;;;; laid out in *pixels* by `crates/gui', with its text measured and wrapped by
;;;; the real font instead of counted in columns. So a heading is genuinely
;;;; larger without its whole line being larger, a table's cells are boxes and
;;;; the emphasis inside one is drawn, a wrapped list item hangs under its own
;;;; text rather than under the bullet, a rule knows how wide the pane is, and a
;;;; drawer is not hidden — it is simply never built.
;;;;
;;;; **The scanner did not move.** `%org-frozen-scan' still answers one role per
;;;; line and a block list, `%org-frozen-cells' still splits a table row,
;;;; `%org-modern-inline' still finds the emphasis and the links, and none of
;;;; them knows the word "scene" any more than they knew the word "overlay".
;;;; What changed is the back end underneath them: a `case' that used to answer
;;;; overlay properties now answers `run' keywords, and a `dotimes' that used to
;;;; make overlays now builds nodes. That the port came out this way is not luck
;;;; — this file was written with the scan and the drawing separated on purpose,
;;;; and said so.
;;;;
;;;; It is still *org-modern plus more*, not a second scanner. `org-frozen-mode'
;;;; derives from `org-mode', so entering it runs org-mode's own body first, and
;;;; everything below is what it then puts on top.
;;;;
;;;; The three things it does not do, and why:
;;;;
;;;;   - **No heading numbering.** It reads well in a book and badly here: the
;;;;     documents this exists for — `docs/curriculum.org' and the tutor — write
;;;;     their own numbers into the Contents links (`[[id:unit-1][1. Vectors]]'),
;;;;     so generated numbers would sit beside hand-written ones that disagree
;;;;     the first time a unit is reordered.
;;;;   - **No captions.** `org-modern.lisp' already declines `#+CAPTION:' at
;;;;     length; a caption is a line of prose under a figure and already reads
;;;;     as one.
;;;;   - **No export.** This renders the buffer you have open. Writing a PDF is
;;;;     a different program.
;;;;
;;;; ponytail — the page is rebuilt whole, every time. There is no incremental
;;;; path and there should not be: the document is read-only, so the only things
;;;; that can invalidate it are entering the mode, a theme change, and a program
;;;; writing into the buffer. The cost is one `buffer-string', one scan, and one
;;;; `highlight' per source block, which is a curriculum-sized file in well
;;;; under a frame. `crates/gui' lays the result out whole for the same reason
;;;; and says so in its own ceiling list.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; What a printed page looks like
;;;
;;; The whole of this mode's taste, in parameters. All DEFPARAMETER, so a config
;;; can set one and press `SPC m m' without reloading anything — the same
;;; contract `org-modern.lisp' offers for its bullets.
;;;
;;; The units changed with the back end and it is worth saying out loud: these
;;; used to be *characters* and are now *pixels*, because a scene is laid out in
;;; pixels and `crates/gui' is the thing that knows what a character is.
;;;
;;; And a scene's pixels are **device** pixels. `Length::Px' reaches the layout
;;; engine and the layout engine measures through the renderer's own faces,
;;; which are opened at the *scaled* point size — so on a 2× display a `:pad 24'
;;; is 24 physical pixels and looks like 12, while `*font-size*' 16 stays 16. A
;;; scene has no em, and Lisp cannot ask for one: *measuring a string in a real
;;; font* is in the Rust column of the boundary. `*org-frozen-scale*' below is
;;; the one number that closes the gap, and everything else here is written in
;;; the logical pixels a config author actually thinks in.
;;;
;;; Where a length can be a *proportion* instead it is one, because a proportion
;;; needs no scale at all: the rule, the page's measure and every table column
;;; are percentages, and the two places that could not be — the space between
;;; things, and padding — are the two places this parameter is spent.

(defparameter *org-frozen-code-face* "modeline"
  "Face whose *colour* is the band behind a source block.

A face name and not an RGB triple, because that is the only colour vocabulary a
scene has — `Rect { fill: Option<FaceId> }' and `Block::background' are both
numbers naming an entry of `face-list', for the reason an overlay's colours are
names: the theme owns the colours, so a page follows `load-theme' for free.

`modeline' is borrowed rather than earned: there is no `code-background' face,
and the modeline's is the one colour in the theme already chosen to sit *behind*
text at low contrast. The cost is that a config restyling its modeline restyles
this band with it.

ponytail: a document wants *several* distinct tints — a code band, a quote, a
`todo' pill, a `done' pill, a placeholder, a table header — against a face list
with one plausible candidate, and the scene made that worse rather than better
by making all six easy to ask for. Ceiling: two of them are the same colour. The
upgrade path is the one already written down for overlays — a primitive taking
three floats, the shape `set-syntax-color' has — and this is the first thing
that genuinely needs it.")

;;; **A glyph that stands in for punctuation is drawn in the coding face.**
;;;
;;; Three of them — a heading's bullet, a list's marker, a checkbox's cookie —
;;; and all three take `:family :mono' while the words beside them are prose.
;;; That looks inconsistent and is the opposite: `*org-modern-stars*',
;;; `*org-modern-list-bullets*' and `*org-modern-checkboxes*' are org-modern's
;;; tables, and org-modern draws on the *cell grid*, so every glyph in them was
;;; chosen against the coverage of the font the grid is drawn in. `◉', `✸', `✿',
;;; `❀', `✜' and `☐' are not characters a reading serif has any reason to carry,
;;; and a scene that asked Charter for one got a blank where the bullet should
;;; be — which is what "org-modern's bullets do not work in org-frozen" looked
;;; like, and the only thing that was actually wrong with them.
;;;
;;; Reading the tables rather than copying them is what makes this the right fix
;;; rather than a second list of glyphs that happen to render: a config that
;;; sets its own bullets sets them in both modes, and they arrive here in the
;;; font they were picked for.

(defparameter *org-frozen-heading-faces* '("heading-1" "heading-2" "heading-3")
  "Face per heading level, from the top; a level past the end takes the last.

The faces org's own highlighter already paints a headline in, so a frozen page
and a manuscript agree about what colour a heading is and a theme has nothing
new to style.")

(defparameter *org-frozen-title-scale* 2.0
  "Type size of a `#+TITLE:' line, as a multiple of the body's.

Twice the body, against 1.5 for a level-1 heading, because a title is not a
heading: it names the document rather than a part of it, and the gap between it
and the first heading is what says so. NIL leaves it at body size.

A *multiple* rather than a point size, and it reaches Rust as the percentage
`:size 200' — the unit a run's size is written in, because the bridge to Lisp is
integers. **The renderer snaps a size to one of four steps**, which is what
bounds its glyph cache, so this parameter means whichever step it lands on: 2.0
and 1.75 are very likely the same face, and 1.15 and 1.0 certainly are. That is
a real ceiling and it is `crates/render''s, not this file's — see the four-step
ladder in `docs/boundary.org'. A document with six distinct type sizes in it
cannot have them.")

(defparameter *org-frozen-subtitle-scale* 1.15
  "Type size of a `#+SUBTITLE:' line. Barely above the body on purpose: a
subtitle that competes with the title is not a subtitle. See the snapping note
on `*org-frozen-title-scale*' — this one very probably lands on the body step,
and the italic and the comment face are what actually distinguish it.")

(defparameter *org-frozen-rule-width* 60
  "Percentage of the page's measure a horizontal rule (`-----' alone on a line)
is drawn across.

A percentage, and that is the ceiling this port *closed*. It used to be a count
of `─' characters, 72 of them, with a docstring explaining that the image cannot
know the pane's width and that the one number it could use is the wrong one. A
scene's lengths include `(pct N)', resolved against the parent's content box by
the layout engine, so the rule is now a `rect' as wide as the measure says and
the parameter means what it says.")

(defparameter *org-frozen-measure* 74
  "Percentage of the pane the page's text column occupies, centred.

A book is not typeset edge to edge, and prose set across a wide monitor is prose
nobody finishes a line of. The remaining space is two `rect' spacers either
side, which is the `Fill | Pct | Fill' shape `docs/gui.org' names as its
worked example of a centred measure.

ponytail: a percentage and not a maximum, so a narrow pane gets a narrow column
where it should get all of itself. Ceiling: reading this in a split. The upgrade
path is a `Length::Max' or a min of two lengths in `crates/gui', which is one
arm of `fixed' and nothing else.")

(defparameter *org-frozen-scale* 2
  "Device pixels per logical pixel — your display's scale factor.

The one number on this page Lisp is not allowed to know. A scene's lengths are
device pixels, because they reach a layout engine that measures through faces
the renderer opened at the *scaled* point size; `*font-size*' is the unscaled
one, so no arithmetic here can recover the ratio. 2 for a HiDPI screen, which is
every laptop this is developed on; **1 on an ordinary one**, and everything
below then means what it says.

ponytail: a config's problem, and it should not be. Ceiling: a page laid out at
half or twice the intended spacing on a display this guesses wrong about — legible
either way, since nothing here depends on a length being exact. Two upgrade
paths and the second is the real one: a reader answering the renderer's scale,
or `Length' growing an `Em' case, which is what `*org-image-width*' already does
for a figure and for exactly this reason — *the device pixel is something only
the renderer knows*.")

(defun %org-frozen-px (n)
  "N logical pixels as the device pixels a scene's lengths are in."
  (max 0 (round (* n *org-frozen-scale*))))

(defparameter *org-frozen-page-pad* 24
  "Logical pixels of margin inside every edge of the page.")

(defparameter *org-frozen-gap* 8
  "Logical pixels between two things on the page — two paragraphs, a paragraph
and a table. `:gap' is *between* children and never around them, so the page's
own margin is `*org-frozen-page-pad*' and this never doubles up with it.")

(defparameter *org-frozen-heading-space* 14
  "Extra logical pixels above a heading, on top of the ordinary gap.

Space above a heading is the thing the grid could not do at all — a row is a row
— and it is most of what makes a page look printed: what says \"a new section
starts here\" is the white above it, not the size of the type.")

(defparameter *org-frozen-code-pad* 10
  "Logical pixels between a source block's band and the code inside it.

The gutter a printed listing has, and it replaces the two-space `line-prefix'
the grid version drew in the left margin of every row. Padding rather than a
prefix is the whole difference: a prefix is drawn on *every* row including
wrapped continuations, which is right for a listing and wrong for a list item —
see `%org-frozen-list-node', which is where that bug got fixed.")

(defparameter *org-frozen-quote-pad* 20
  "Logical pixels a `#+begin_quote' is inset by, on every side. This and the
comment face are the whole of what says \"quotation\" — see the note in
`%org-frozen-block-node' for the third thing that used to.

A real inset *measure* — the quote is narrower than the prose around it, which
is what a quote in a book is — rather than the same width pushed right, which is
all a `line-prefix' could do.

ponytail: no rule down the left. A rule beside a quote is a `rect' of a fixed
width and the *block's own height*, and `Length::Fill' is honoured along a
block's stacking axis only, so a `:height fill' inside a `:dir :row' measures
against whatever height was offered from above and would make the quote as tall
as the pane. Ceiling: a quote reads as an inset paragraph and not as a quoted
one. The upgrade path is `Fill' resolved across the stacking axis too, which is
one clause in `children' in `crates/gui/src/layout.rs'.")

(defparameter *org-frozen-marker-gap* 8
  "Logical pixels between a list item's marker and its text.

The marker's *box* is sized to the marker, and this is the space after it. The
item's text starts on the far side and — this is the point — *every* line of it
starts there, wrapped continuations included, because the marker and the body
are two children of a row and the body is a box of its own. That hanging indent
is one of the things the grid could not do: `line-prefix' draws in the margin of
every row of a line, first row included, so a wrapped list item's second line
used to start at column 0.

A nested item is indented with *spaces in front of its marker* rather than with
a wider box, because a space is measured by the font and a pixel is not: the
indent then matches the source whatever the display's scale is.")

(defparameter *org-frozen-list-gap* 4
  "Logical pixels between two items of the same list — tighter than
`*org-frozen-gap*', because a list is one thing and not five.")

(defparameter *org-frozen-table-row-gap* 4
  "Logical pixels between two rows of a table.")

(defparameter *org-frozen-table-pad* 3
  "Characters of gutter added to every table column's share of the measure.

Which is to say: **no vertical rules**. That is the printed convention — a
book's table has horizontal rules and white space, and the vertical lines in an
org table are there because a text editor has no other way to say where a column
ends. Once the columns are actually aligned they are noise. A cell's box is this
much wider than its widest text and the slack falls on the right, which is the
gutter.

*Characters*, because the widths this feeds are **percentages**. `docs/gui.org'
is explicit that a column's width is a constraint across siblings — the widest
cell of that column in *every* row — and that no per-child `Length' expresses
it, so the writer computes the widths once and every row states the same ones;
`:auto' would size each cell to its own content and the table would not line up
at all. What the document does *not* say is which unit to state them in, and
pixels are the wrong answer: Lisp cannot measure a font, so a pixel width is a
character count times a guess, and a guess that comes out low wraps every cell.
A percentage needs no guess — each column takes the share of the measure its
content asks for, and the shares are equal in every row by construction.

ponytail: a table therefore always fills the measure, so a two-column table of
short cells is spread out rather than compact. And a *character* is not a width
— a table's cells are prose, and prose is set in the proportional face, where
`W' and `i' are a factor of three apart — so a column's share is proportional to
what it says rather than to what it draws. Every row still states the same
shares, so the columns line up whatever the estimate; a column the estimate was
mean to wraps, and nothing clips. Ceiling: a table of prose, or one column of
capitals beside one of narrow letters. The upgrade path is the fifth node kind
`docs/gui.org' names, `Grid', which is the only thing that makes a column's
width a fact about the column rather than about its cells.")

(defparameter *org-frozen-src-languages*
  '(("elisp" . "lisp") ("emacs-lisp" . "lisp") ("common-lisp" . "lisp")
    ("cl" . "lisp") ("scheme" . "lisp") ("racket" . "lisp")
    ("js" . "javascript") ("jsx" . "javascript") ("node" . "javascript")
    ("py" . "python") ("python3" . "python")
    ("jsonc" . "json") ("h" . "c") ("cpp" . "c") ("c++" . "c"))
  "`#+begin_src NAME' -> the language id `highlight' knows it by.

Org's babel names and tree-sitter's language ids are two vocabularies that
mostly agree, and this is the disagreement. A name with no entry is passed
through unchanged, and a name no grammar answers to — `sh', `text', `org-mode'
— highlights as nothing at all rather than as an error: `highlight' answers an
empty list for a language it has never heard of, which is the same answer it
gives for a parse failure, so there is one case to handle and not three.")

(defun %org-frozen-block (name)
  "What a `#+begin_NAME' body looks like, as a plist of flags.

The `case' survived the port and its *vocabulary* did not, which is the whole
shape of this change in one function. It used to answer overlay properties —
`line-background', `line-prefix', `face', `slant' — mixed in with two flags the
caller read for itself. It now answers only flags, because a scene's version of
\"a band the width of the pane\" is a `block' with a `:background' and its
version of \"a rule down the left\" is a node, and neither is a property name a
table can hold.

  :HIGHLIGHT  set the body in its own language;
  :BAND       a tinted panel with a gutter — a listing;
  :QUOTE      inset, italic, in the comment face;
  :HIDE       the body is not part of the page at all.

Giving a kind of block a new look is still a line in its arm and no change to
anything that builds it, which is the property that made the port cheap.

The fallback is a band. A block of an unknown kind is *some* kind of set-apart
text, and drawing it as ordinary prose would lose the one thing its author was
certainly saying."
  (cond
    ((string-equal name "src") (list :highlight t :band t))
    ((member name '("example" "verse") :test #'string-equal) (list :band t))
    ((string-equal name "quote") (list :quote t))
    ;; `#+begin_comment' is a note to the author and `#+begin_export' is
    ;; someone else's markup. Neither is on the page.
    ((member name '("comment" "export") :test #'string-equal) (list :hide t))
    (t (list :band t))))

;;; ---------------------------------------------------------------------------
;;; Buffer text on its way back out
;;;
;;; **Every string this file takes out of the buffer goes through
;;; `%org-frozen-text' first.** That is one function and one bug, and the bug is
;;; worth the words because it is invisible: the page is drawn correctly right
;;; up until somebody puts an em dash in the title — which the curriculum in
;;; `examples/math/' does, in its first line.
;;;
;;; The shape of it. Lisp holds buffer text as UTF-8 **bytes**, one character per
;;; byte: that is the deliberate choice `f_query' in `shim.c' writes up, and it
;;; is what makes `search-forward' and `%char-index' work. `(buffer-string)'
;;; therefore answers a `(SIMPLE-ARRAY CHARACTER)' in which `—' is *three*
;;; characters, one per byte, each under 256. Going the other way, `dup_utf8'
;;; encodes a character string — one UTF-8 sequence per character — which is
;;; right for a literal a `load'ed file gave it and is exactly wrong for this:
;;; three already-encoded bytes come out as six, and the em dash is drawn as the
;;; Latin-1 reading of its own encoding.
;;;
;;; The scene made this *mandatory* rather than optional, and that is the one way
;;; the port raised the stakes. Every string a scene hands over is measured by
;;; `Measure::advance' in a real font, so bytes reaching it are not merely drawn
;;; wrong — they are three characters wide where the document has one, and every
;;; wrap, every column width and every centred line is computed from the wrong
;;; number.
;;;
;;; So this file decodes **once, per line, at the top of the builder**, and
;;; everything downstream — the emphasis scanner, the table's column arithmetic,
;;; the slices handed to `run' — works in characters. That is simpler than what
;;; the overlay version did, which had to keep byte indices to hand `make-overlay'
;;; and converted with `%char-index' at every use.
;;;
;;; ponytail: this is the local repair and not the general one, and there are now
;;; *two* copies of it — `utf8-text' in `runtime/gui.lisp' is the same body,
;;; written because that file needed it before this one could be depended on.
;;; They agree today. The cheap upgrade path is this function becoming a call to
;;; that one, which loads first; the coherent fix underneath both is the one
;;; `docs/boundary.org' names on `f_query' — decode in the shim and delete the
;;; byte model entirely.

(defun %org-frozen-text (bytes)
  "BYTES — buffer text, one character per UTF-8 byte — as the characters it
actually spells.

ASCII is returned unchanged and untouched, which is both the fast path and the
common one: a document with no accent in it never allocates here.

An invalid or truncated sequence is passed through one character at a time
rather than replaced or dropped. This is a *renderer*: text it cannot make sense
of should still be visible, because the alternative is a page that silently
disagrees with the file behind it."
  (let ((n (length bytes)))
    (if (every (lambda (c) (< (char-code c) 128)) bytes)
        bytes
        (with-output-to-string (out)
          (let ((i 0))
            (loop while (< i n)
                  do (let* ((b (char-code (char bytes i)))
                            (len (cond ((< b #x80) 1)
                                       ((< b #xC0) 0) ; a stray continuation byte
                                       ((< b #xE0) 2)
                                       ((< b #xF0) 3)
                                       (t 4)))
                            (code (cond ((< b #x80) b)
                                        ((< b #xE0) (logand b #x1F))
                                        ((< b #xF0) (logand b #x0F))
                                        (t (logand b #x07)))))
                       (cond
                         ((or (zerop len) (> (+ i len) n))
                          (write-char (char bytes i) out)
                          (incf i))
                         (t
                          (loop for k from 1 below len
                                do (setf code
                                         (logior (ash code 6)
                                                 (logand (char-code (char bytes (+ i k)))
                                                         #x3F))))
                          (write-char (code-char code) out)
                          (incf i len))))))))))

;;; `%org-frozen-repeat' used to live here: N copies of a character, with a
;;; comment about `:element-type 'character' because a base string cannot hold
;;; `─'. It existed to draw a horizontal rule and a table's rule row out of box
;;; characters, one glyph per cell, because a grid has nothing else to draw a
;;; line with. A scene has `rect' — a box with a width, a height and a colour —
;;; so both callers are one node each and the function has nothing left to do.

;;; ---------------------------------------------------------------------------
;;; Reading a line of org, the parts org-modern had no reason to
;;;
;;; Everything inline — emphasis, links, checkboxes, bullets — is already read
;;; by `org-modern.lisp' and is deliberately not read again here. What follows
;;; is the *machinery*: the lines a manuscript has and a printed page does not.

(defun %org-frozen-keyword (line name)
  "The value of LINE when it is the keyword line `#+NAME: value', else NIL.

An empty value answers the empty string rather than NIL, exactly as
`%org-property' does and for the same reason: `#+TITLE:' with nothing after it
is a different thing from a line that is not a title at all, and the caller
wants to tell them apart before it decides whether to draw anything.

Case-insensitive, because org is: `#+title:' and `#+TITLE:' are one keyword."
  (let* ((n (length line))
         (i (or (position-if-not #'%org-blank-p line) n))
         (tag (concatenate 'string "#+" name ":"))
         (m (length tag)))
    (when (and (<= (+ i m) n) (string-equal tag line :start2 i :end2 (+ i m)))
      (string-trim '(#\Space #\Tab #\Return) (subseq line (+ i m))))))

(defun %org-frozen-block-open (line)
  "(NAME . LANG) when LINE is a `#+begin_NAME lang ...' line, else NIL.

NAME is downcased and LANG is NIL when the block names none — which is every
block except `src', and is why they are one function rather than two: the
caller has to know both to decide what the body looks like, and asking twice
would mean parsing the line twice."
  (let* ((n (length line))
         (i (or (position-if-not #'%org-blank-p line) n))
         (tag "#+begin_")
         (m (length tag)))
    (when (and (<= (+ i m) n) (string-equal tag line :start2 i :end2 (+ i m)))
      (let* ((from (+ i m))
             (name-end (or (position-if #'%org-blank-p line :start from) n))
             (lang-start (or (position-if-not #'%org-blank-p line :start name-end) n))
             (lang-end (or (position-if #'%org-blank-p line :start lang-start) n)))
        (cons (string-downcase (subseq line from name-end))
              (when (< lang-start lang-end)
                (string-downcase (subseq line lang-start lang-end))))))))

(defun %org-frozen-drawer-p (trimmed)
  "True when TRIMMED — a line with its indentation already off — opens a
property drawer: `:PROPERTIES:', `:LOGBOOK:', anything of that shape.

Org's own rule is `:WORD:' alone on a line, and applying the rule rather than
listing the names is what makes `:LOGBOOK:' and a config's own drawer work
without an entry anywhere. `:END:' is excluded by name because it *closes* one
and would otherwise open a second that never ends — which, given that an
unterminated drawer hides the rest of the file, is the one mistake here worth
guarding against explicitly."
  (let ((n (length trimmed)))
    (and (> n 2)
         (char= (char trimmed 0) #\:)
         (char= (char trimmed (1- n)) #\:)
         (not (string-equal ":END:" trimmed))
         (every (lambda (c) (or (alphanumericp c) (char= c #\_)))
                (subseq trimmed 1 (1- n))))))

(defun %org-frozen-rule-p (trimmed)
  "True when TRIMMED is org's horizontal rule: five or more dashes and nothing
else. Five is org's own threshold, and it is what keeps `----' inside a table's
`|---+---|' — which never reaches here anyway — and a run of three dashes in
prose from becoming a rule."
  (and (>= (length trimmed) 5)
       (every (lambda (c) (char= c #\-)) trimmed)))

(defun %org-frozen-tags (line)
  "Index in LINE where a headline's trailing `:tag:tag:' run begins, counting
the whitespace in front of it, or NIL.

The whitespace goes with the tags deliberately: a heading's text ends before it,
and the page has no use for either.

The function did not change and its *contract* did. It used to answer a byte
index, because `line-string' hands out UTF-8 bytes and every caller converted
with `%char-index' to reach `make-overlay'. A scene has no buffer offsets at
all, so the builder decodes each line before it reads it and this answers an
index into that decoded string — which is what `subseq' wants, and is the same
arithmetic either way, since a tag alphabet is ASCII."
  (let ((e (length line)))
    (loop while (and (plusp e) (%org-blank-p (char line (1- e)))) do (decf e))
    (let ((s (position-if #'%org-blank-p line :end e :from-end t)))
      (when (and s
                 (> (- e s) 3)             ; `:x:' at the very least
                 (char= (char line (1+ s)) #\:)
                 (char= (char line (1- e)) #\:)
                 ;; Org's tag alphabet, and no empty tag: `::' would make
                 ;; `a :: b' at the end of a heading read as a tag run.
                 (every (lambda (c)
                          (or (char= c #\:) (alphanumericp c)
                              (member c '(#\_ #\@ #\#))))
                        (subseq line (1+ s) e))
                 (not (search "::" (subseq line (1+ s) e))))
        (let ((w s))
          (loop while (and (plusp w) (%org-blank-p (char line (1- w)))) do (decf w))
          w)))))

(defun %org-frozen-tag-list (run)
  "The `:a:b:' RUN of a headline as the list (\"a\" \"b\"), or NIL.

Split rather than drawn: what the tags become is the *builder's* decision and
the hook's, not this function's — see `*org-frozen-node-functions*', which is
handed this list."
  (let ((s (string-trim '(#\Space #\Tab #\Return #\:) run)))
    (when (plusp (length s))
      (let ((out nil) (start 0))
        (loop for i = (position #\: s :start start)
              do (push (subseq s start (or i (length s))) out)
                 (if i (setf start (1+ i)) (return)))
        (remove "" (nreverse out) :test #'string=)))))

(defun %org-frozen-language (name)
  "NAME as a language id `highlight' will answer to, or NIL for no name at all."
  (when (and name (plusp (length name)))
    (let ((n (string-downcase name)))
      (or (cdr (assoc n *org-frozen-src-languages* :test #'string=)) n))))

;;; ---------------------------------------------------------------------------
;;; One pass, one role per line
;;;
;;; Everything below draws from a single classification of the buffer, and that
;;; is the design rather than an optimisation. Five passes each deciding for
;;; themselves what a `#+begin_src' line is would be five chances to disagree
;;; about it — and under overlays that meant a blank row with a glyph on it,
;;; while under a scene it means a node in the wrong place, which is worse
;;; because there is no row for it to be wrong *in*.
;;;
;;; Line-oriented for org-modern's reason: org decides what a line *is* from its
;;; first few characters, and only blocks and drawers carry state across lines.

(defun %org-frozen-lines ()
  "The buffer as a vector of (STRING BEGIN END), which is `%org-lines' with
random access. A vector and not the list, because the grouping pass asks about
lines by index and doing that with NTH over a textbook is quadratic for no
reason at all."
  (coerce (%org-lines) 'vector))

(defun %org-frozen-scan (v)
  "(ROLES . BLOCKS) for the line vector V.

ROLES is a vector parallel to V, holding one of:

  :HIDDEN    machinery — a drawer, a keyword, a `#+begin_'/`#+end_' delimiter,
             a `# ' comment. A printed page shows none of it.
  :HEADING   a headline, which may carry tags to take off.
  :TITLE     `#+TITLE:', typeset rather than hidden.
  :SUBTITLE  `#+SUBTITLE:', likewise.
  :BLOCK     inside a block's body — claimed by the block, so no other pass
             touches it.
  :TABLE     a `|' row.
  :RULE      `-----'.
  NIL        prose, which is org-modern's business and not this file's.

BLOCKS is (NAME LANG FIRST LAST) per block, in order, with FIRST and LAST the
line indices of its *body* — FIRST > LAST for an empty one, which is what the
drawing pass tests rather than a separate flag. The `#+begin_' line is therefore
always FIRST - 1, which is how `%org-frozen-groups' finds a block from the line
it starts on.

An unterminated block runs to the end of the file. That is the same call
`%org-modern-scan' makes, and for the same reason: guessing where the author
meant it to stop produces a document that is wrong somewhere you cannot see."
  (let* ((n (length v))
         (roles (make-array n :initial-element nil))
         (blocks nil)
         (name nil) (lang nil) (first-line nil)
         (in-drawer nil))
    (dotimes (i n)
      (let* ((line (first (aref v i)))
             (trimmed (string-trim '(#\Space #\Tab #\Return) line)))
        (cond
          ;; Inside a block, the *only* question is whether this closes it.
          ;; Nothing else is looked at, which is what keeps a `|' in a shell
          ;; heredoc from being a table row and a `-----' in a diff from being
          ;; a rule.
          (name
           (cond ((%org-directive-p line "#+end_")
                  (setf (aref roles i) :hidden)
                  (push (list name lang first-line (1- i)) blocks)
                  (setf name nil))
                 (t (setf (aref roles i) :block))))
          ((%org-frozen-block-open line)
           (let ((open (%org-frozen-block-open line)))
             (setf (aref roles i) :hidden
                   name (car open)
                   lang (cdr open)
                   first-line (1+ i))))
          (in-drawer
           (setf (aref roles i) :hidden)
           (when (string-equal ":END:" trimmed) (setf in-drawer nil)))
          ;; Before the drawer test, because a headline is never a drawer and
          ;; `* :notes:' would otherwise open one.
          ((%org-line-level line) (setf (aref roles i) :heading))
          ((%org-frozen-drawer-p trimmed) (setf (aref roles i) :hidden
                                                in-drawer t))
          ((%org-frozen-keyword line "TITLE") (setf (aref roles i) :title))
          ((%org-frozen-keyword line "SUBTITLE") (setf (aref roles i) :subtitle))
          ;; Every other `#+keyword:' and every `#' comment. Two directives and
          ;; one arm, because the question is the same: is this a line the
          ;; author wrote *about* the document rather than *in* it.
          ((%org-directive-p line "#") (setf (aref roles i) :hidden))
          ((%org-frozen-rule-p trimmed) (setf (aref roles i) :rule))
          ((and (plusp (length trimmed)) (char= (char trimmed 0) #\|))
           (setf (aref roles i) :table))
          (t nil))))
    (when name (push (list name lang first-line (1- n)) blocks))
    (cons roles (nreverse blocks))))

;;; ---------------------------------------------------------------------------
;;; Inline markup, as runs
;;;
;;; The second back end, behind a front end that did not move. `%org-modern-line'
;;; and `%org-modern-inline' answer *(BEG END KIND)* and know nothing about how
;;; a kind is drawn; `%org-modern-render' turns a kind and its literal text into
;;; a plist of **overlay** properties, and its own docstring argues for exactly
;;; the seam this port needed. `%org-frozen-markup' below is that function's
;;; twin for a scene: same input, `run' keywords instead of overlay properties.
;;;
;;; It is an *addition* and not a replacement, which is the one place this file
;;; disagrees with the survey it was written from. An ordinary org buffer is
;;; still a grid with a cursor in it, so `%org-modern-render' still has a
;;; customer; there are two back ends because there are two surfaces.
;;;
;;; What a scene buys here, and the grid could not:
;;;
;;;   - **emphasis inside a table cell.** `%org-modern-line' skips a `|' row
;;;     whole, for the stated reason that `|--+--|' would read as a `+strike+'
;;;     run, and the grid version of this file then rebuilt each row as one flat
;;;     `display' string — so `| *bold* |' drew as the six literal characters.
;;;     A cell is a paragraph like any other here and the scanner runs over it.
;;;   - **emphasis inside a heading.** `%org-modern-line' skips a headline too,
;;;     because the highlighter paints a heading line in one face and two things
;;;     would claim the same cells. There are no cells.
;;;   - **a run set in its own size.** `Overlay::scale' is a line property; a
;;;     `run' has its own.
;;;
;;; Everything here works on **decoded** text — see `%org-frozen-text'. Offsets
;;; are therefore character offsets into a character string, and there is no
;;; `%char-index' anywhere below this line.

(defun %org-frozen-merge (base extra)
  "BASE, a plist of `run' keywords, with everything EXTRA also names replaced.

Merged rather than concatenated so the printed form carries each keyword once.
Both orders parse the same — the reader takes the last of a repeated keyword —
but a page is source somebody may have to read."
  (let ((out (copy-list base)))
    (loop for (k v) on extra by #'cddr do (setf (getf out k) v))
    out))

(defun %org-frozen-follow (target)
  "Follow the link TARGET, which is `org-open-at-point' with the *point* taken
out of it.

A scene has no cursor, so `%org-link-at-point' has nothing to look under; what a
link is instead is a `:tag' on its own run, and a click arrives here with the
target the closure captured. The policy table is unchanged and is org-modern's
— a config that adds `doi:' gets it in both places.

ponytail: `org-link-open-id' and `%org-link-open-plain' both end in
`goto-char', which moves a point nothing is drawing. So following an `id:' link
inside a frozen page silently does nothing visible. Ceiling: the Contents
section of a curriculum, which is the highest-traffic link in the corpus. The
upgrade path is `docs/gui.org''s own gap — bringing a *node* into view, which
`Layout::rect_of' already answers and nothing yet asks for."
  (let* ((scheme (car (%org-link-scheme target)))
         (open (and scheme (cdr (assoc scheme *org-link-open-functions*
                                       :test #'string=)))))
    (if (and open (fboundp open))
        (funcall open target)
        (%org-link-open-plain target))))

(defun %org-frozen-markup (kind text)
  "The (STRING . PLIST) a KIND substitution of the literal TEXT draws as, or NIL
when TEXT is not that kind of markup any more.

STRING is what goes on the page and PLIST is the `run' keywords that set it —
the scene twin of `%org-modern-render', and the same NIL-means-retire contract,
for the same reason: a scanner and a renderer that disagree should produce the
text as written rather than a glyph for something that is not there."
  (let ((n (length text)))
    (case kind
      (:emphasis
       (let ((face (and (> n 2) (cdr (assoc (char text 0) *org-modern-emphasis*)))))
         (when (and face (char= (char text 0) (char text (1- n))))
           (cons (subseq text 1 (1- n))
                 ;; Really bold and really italic, rather than the colour the
                 ;; face table had to stand in for. Read off the face name and
                 ;; not from a second table: two would be one mapping written
                 ;; twice, and would drift. The face stays either way — it is
                 ;; what carries `code' and `comment'.
                 ;;
                 ;; ...and `=verbatim=' takes the *coding face* as well as the
                 ;; colour, which is what `code' means in any printed book and
                 ;; what a scene can do for the first time: an identifier in the
                 ;; middle of a sentence is set in the font identifiers are
                 ;; written in. On the grid every character was already that
                 ;; font, so there was nothing to say.
                 (append (list :face face)
                         (cond ((string= face "bold") (list :bold t))
                               ((string= face "italic") (list :italic t))
                               ((string= face "code") (list :family :mono))))))))
      (:link
       (let ((parts (%org-link-parts text)))
         (when parts
           (let ((target (car parts)))
             (cons (cdr parts)
                   ;; The whole of "a link is clickable", and it is one keyword.
                   ;; The closure is written where the widget is, which is the
                   ;; shape `runtime/gui.lisp' asks for; the integer it becomes
                   ;; is the only thing that crosses into Rust.
                   (list :face "link"
                         :tag (lambda () (%org-frozen-follow target)))))))))))

(defun %org-frozen-pieces (text &optional (from 0))
  "TEXT from FROM as a list of (STRING . PLIST) — what to draw, and how.

One scanner pass with two consumers: `%org-frozen-runs' turns it into a
paragraph's runs, and `%org-frozen-drawn' concatenates the strings to answer how
wide the result will be. A table's column arithmetic needs the second, and
getting it by scanning twice would be two chances to disagree about what a cell
says.

TEXT must already be characters. See `%org-frozen-text'."
  (let ((out nil) (i from) (n (length text)))
    (dolist (s (%org-modern-inline text from))
      (destructuring-bind (a b kind) s
        (when (< i a) (push (cons (subseq text i a) nil) out))
        (push (or (%org-frozen-markup kind (subseq text a b))
                  (cons (subseq text a b) nil))
              out)
        (setf i b)))
    (when (< i n) (push (cons (subseq text i n) nil) out))
    (nreverse out)))

(defun %org-frozen-runs (pieces &optional style)
  "PIECES as `run's, every one of them set in STYLE first.

STYLE is what a *context* claims — a heading's size, a table header's weight —
and a piece's own keywords win over it, which is what lets `*bold*' inside a
heading be bold-inside-a-heading rather than body-weight text.

A piece whose car is :IMAGE is a typeset equation and becomes an `image-run'
instead: a bitmap that wraps like a word and hangs off the line's baseline. It
takes no style, because a rendered equation already carries the colour and the
size it was rasterised at — `latex-preview' asks the editor for both.

**Adjacent pieces set the same way become one run**, and that is not thrift.
`crates/gui' measures a paragraph one whitespace-delimited segment at a time and
accumulates the width, while `crates/render' *draws* a run's segments as a
single string — so a run's drawn width is `advance(whole)' and its recorded
width is the sum of `advance(part)', and the two are not equal: kerning makes
the sum larger and an italic's shear makes it smaller. The error is invisible
inside a run, because everything in one run is drawn from one origin, and lands
in full at the *boundary* between two. Every boundary this file does not create
is one place the words cannot come out overspaced or squashed together.

It is a mitigation and not a fix — a `*bold*' in the middle of a sentence is a
boundary the document asked for and no coalescing can remove it. The fix is one
crate out and is small: `emit' in `crates/gui/src/layout.rs' should set a merged
piece's width from `m.advance' of the text it now covers, rather than adding the
part it just took on, so that what layout records and what the painter draws are
the same number."
  (let ((merged nil))
    (dolist (p pieces)
      (cond
        ((eq (car p) :image) (push (cons :image (cdr p)) merged))
        ((plusp (length (car p)))
         (let ((look (%org-frozen-merge style (cdr p)))
               (top (first merged)))
           (if (and top (not (eq (car top) :image)) (equal (cdr top) look))
               (setf (car top) (concatenate 'string (car top) (car p)))
               (push (cons (car p) look) merged))))))
    (let ((out nil))
      (dolist (m (nreverse merged) (nreverse out))
        (push (if (eq (car m) :image)
                  (image-run (getf (cdr m) :id))
                  (apply #'run (car m) (cdr m)))
              out)))))

(defun %org-frozen-drawn (pieces)
  "What PIECES put on the page, as one string — the markers already off.

An equation answers its own *source*, which is a guess and the only one
available: this is what a table column's width is computed from, and how wide
`$\\lambda$' comes out is a question about a bitmap this file cannot see. Seven
characters for a Greek letter is too many, so a column with maths in it is wider
than it needs to be rather than too narrow to hold it — which is the error worth
making, since nothing clips and a cell too narrow wraps."
  (format nil "~{~a~}"
          (mapcar (lambda (p)
                    (if (eq (car p) :image) (getf (cdr p) :source) (car p)))
                  pieces)))

;;; ---------------------------------------------------------------------------
;;; Mathematics
;;;
;;; The construct this whole mode exists for. `examples/math/linear-algebra.org'
;;; has fourteen inline fragments in 206 lines and eight display equations, and
;;; `docs/curriculum.org' specifies `$...$' as the inline form twice. A page that
;;; showed `$\\mathbb{R}^n$' as those characters would be a page nobody reads.
;;;
;;; Two shapes, and the whole of the work is telling them apart:
;;;
;;;   inline    `$x_1$' in the middle of a sentence. An `image-run' — a bitmap
;;;             that wraps like a word, sits on the line's baseline and hangs
;;;             `depth' pixels *below* it, which is what stops a subscript
;;;             floating above the text it belongs to. The `Run::Image' arm of
;;;             `crates/gui' exists for exactly this and this is its first
;;;             customer.
;;;   display   `\\[ ... \\]' alone on its lines. An `image' node inside a
;;;             centring `block' — a figure, with air around it.
;;;
;;; **The bit that decides which was already computed and already thrown away.**
;;; `latex-fragments' answers (START END DISPLAY) and every caller in the tree
;;; ignores the third element: `(declare (ignore display))' at init.lisp:832 and
;;; at the old `%org-frozen-latex' here. An overlay had no use for it — a bitmap
;;; over a range is a bitmap over a range — and a scene has nothing *but* that
;;; use for it.
;;;
;;; Nothing new was needed in Rust. `latex-preview' answers a bare `ImageId' and
;;; hangs nothing, which is what let an overlay use it and what lets a node use
;;; it now; it keys the id on the source, the dpi and the *foreground colour*, so
;;; an equation matches the text around it and follows `load-theme' and `C-+';
;;; and it answers from a cache when it has rendered that fragment before. That
;;; last one is why a page rebuilt whole on every change is affordable at all:
;;; org-mode's own body has already previewed every fragment by the time this
;;; file runs, so every call here is a hash lookup. A cold buffer pays for TeX
;;; once, on the Lisp thread, with the editor still drawing and still taking
;;; keystrokes.
;;;
;;; ponytail: not in a table cell. `%org-frozen-cells' cuts a row up before this
;;; can look at it and the cells no longer know where they were, so a `$x$'
;;; inside `| ... |' is drawn as its source. Ceiling: a table of formulae, which
;;; nothing in the corpus is. The upgrade path is `%org-frozen-cells' answering
;;; each cell's offset alongside its text, which is one more value from a
;;; function that already has it.

(defvar *org-frozen-inline* nil
  "The inline LaTeX fragments of the page being built, as a vector parallel to
the line vector — or NIL when no page is being built.

A special variable and not an argument, deliberately. The alternative is
threading it from `org-frozen-refresh' down to `%org-frozen-line-pieces', which
is seven functions, five of which have no opinion about mathematics at all and
would be carrying it only to hand it on. It is bound for exactly the duration of
one build, so nothing outside a build can see it and two builds cannot
interleave — the image runs one Lisp thread and `org-frozen-refresh' is not
re-entrant.")

(defun %org-frozen-latex-image (source)
  "The id of SOURCE typeset, or NIL when there is no LaTeX to typeset it with.

`*org-latex-auto*' is switched off by a failure, which is the same contract
`%org-latex-render' has and for the same reason: a machine with no `latex'
should say so once, from inside the primitive, rather than once per equation on
a page with forty of them. Asking again by hand — `SPC m m' after installing
TeX — is what turns it back on."
  (when (and (boundp '*org-latex-auto*)
             (symbol-value '*org-latex-auto*)
             (fboundp 'latex-preview)
             (plusp (length source)))
    (let ((id (latex-preview source)))
      (unless id (setf (symbol-value '*org-latex-auto*) nil))
      id)))

(defun %org-frozen-latex-piece (source)
  "SOURCE as an image piece, or as the characters it is written in.

The fallback is the source in the `code' face rather than nothing at all: a
machine with no TeX should show you the equation you wrote, which is readable,
instead of a hole where one was."
  (let ((id (%org-frozen-latex-image source)))
    (if id
        (cons :image (list :id id :source source))
        (cons source (list :face "code" :family :mono)))))

(defun %org-frozen-latex-map (v)
  "The buffer's LaTeX fragments, sorted into the two shapes a page has room for.

Answers (values INLINE DISPLAY):

  INLINE   a vector parallel to V — per line, ((BEG END SOURCE) ...) in indices
           into that line's own *decoded* text, in order.
  DISPLAY  ((FIRST LAST SOURCE) ...), the line ranges a display equation owns
           whole, in document order.

One pass over the fragments beside one pass over the lines, because both are in
order — the cursor into V is never rewound.

No `buffer-substring' anywhere, and that is not thrift. `buffer-substring'
answers UTF-8 *bytes*, so a fragment holding `\\text{café}' would reach TeX
twice-encoded — the same bug `%org-frozen-text' exists to prevent, one layer
further down, where it would come back as a wrongly-typeset equation rather than
as a wrong glyph. The line vector is already decoded and its lines are
consecutive, so joining the lines a fragment lies on with a newline reproduces
the buffer's own offsets exactly and the source is a `subseq'.

A block is opaque to `latex-fragments' — `opaque_ranges' in
`crates/syntax/src/org.rs' covers every `#+begin_'/`#+end_' pair and every
`#+directive' — so nothing here has to exclude a `$' in a shell heredoc or a
`\\[' in a quote, and a fragment can never straddle a block boundary."
  (let* ((n (length v))
         (inline (make-array n :initial-element nil))
         (display nil)
         ;; Not gated on `*org-latex-auto*'. Finding the fragments is one query
         ;; and some arithmetic; *typesetting* one is the expensive half and is
         ;; where the switch lives. And the classification earns its keep either
         ;; way — with no TeX on the machine a display equation still gets a
         ;; line of its own and an inline one is still marked as maths rather
         ;; than left as prose with dollars in it.
         (frags (and (fboundp 'latex-fragments) (latex-fragments)))
         (at 0))
    (dolist (f frags)
      (destructuring-bind (fb fe disp) f
        (loop while (and (< at (1- n)) (< (third (aref v at)) fb)) do (incf at))
        (let ((a at) (b at))
          (loop while (and (< b (1- n)) (< (third (aref v b)) fe)) do (incf b))
          (let* ((base (second (aref v a)))
                 (joined (format nil "~{~a~^~%~}"
                                 (loop for k from a to b
                                       collect (%org-frozen-line v k))))
                 (la (max 0 (min (length joined) (- fb base))))
                 (lb (max la (min (length joined) (- fe base))))
                 (source (subseq joined la lb)))
            (cond
              ;; Display maths with nothing else on its lines is a figure: it
              ;; gets a line of its own, centred, with air above and below.
              ((and disp
                    (%org-frozen-blank-line-p (subseq joined 0 la))
                    (%org-frozen-blank-line-p (subseq joined lb)))
               (push (list a b source) display))
              ;; ...and anything else rides in the sentence it was written in,
              ;; which a run can only do inside one paragraph's flow.
              ((= a b) (push (list la lb source) (aref inline a)))
              ;; A fragment across two lines with prose beside it. There is no
              ;; run that spans two source lines' worth of markup scanning, so
              ;; this is a figure too — rare, and better than dropping it.
              (t (push (list a b source) display)))))))
    (dotimes (i n) (setf (aref inline i) (nreverse (aref inline i))))
    (values inline (nreverse display))))

(defun %org-frozen-latex-node (source)
  "A display equation, centred, with air above and below it.

`(image ID)' and no size: core fills the width, the height and the depth in from
its own image table when the page is installed, which is the whole of why
`docs/gui.org' tells you not to add a `:width' to its example."
  (let ((id (%org-frozen-latex-image source)))
    (if id
        (block :width (pct 100) :align :center
               :pad (%org-frozen-px *org-frozen-gap*)
               (image id))
        (block :width (pct 100) :align :center
               (text (run source :face "code" :family :mono))))))

;;; ---------------------------------------------------------------------------
;;; From a flat vector of roles to a tree
;;;
;;; This is the only genuinely new code in the port, and it is here because of a
;;; mismatch the two designs have with each other: **the scanner answers one
;;; role per line and a scene wants a tree.** Everything the overlay version did
;;; after the scan was a `dotimes' over the vector, which is all a sequence of
;;; overlays ever needs — an overlay is attached to a range and nothing above it
;;; has to exist. A node has a parent.
;;;
;;; What the grouping is: consecutive prose lines with no blank between them are
;;; one paragraph; consecutive `|' rows are one table; a run of list items is one
;;; list; a block is claimed whole from the line its `#+begin_' is on. Machinery
;;; is not grouped at all — it is simply not built, which is what replaced five
;;; functions and a fold.
;;;
;;; What it deliberately is *not*: a section tree. A heading does not contain the
;;; paragraphs under it, and that is a decision rather than a gap — the only
;;; things nesting would buy are an indented subtree and a collapsible one,
;;; neither of which this mode has, and it would put every paragraph of a
;;; textbook inside three blocks that exist to hold it. `%math-headings' resolves
;;; heading extents in one pass over a line list and is what to lift the day a
;;; document asks.

(defun %org-frozen-blank-line-p (line)
  "True when LINE has nothing on it. Newlines count as blank, because
`%org-frozen-latex-map' asks this of a *joined* run of lines."
  (zerop (length (string-trim '(#\Space #\Tab #\Return #\Newline) line))))

(defun %org-frozen-list-item (line)
  "(INDENT BULLET BODY) when LINE opens a list item, else NIL.

INDENT is how far in the marker sits, BULLET is what to draw in its place, and
BODY is where the item's text starts.

Ordered lists are here, and they are a construct the grid declined whole:
`*org-modern-list-bullets*' has no entry for `1.' with the reason written beside
it — *a number is not one character and has no glyph to become*. A substitution
had to be the same width as what it replaced or the text moved; a marker box is
whatever width it is."
  (let* ((n (length line))
         (i (or (position-if-not #'%org-blank-p line) n)))
    (when (< i n)
      (let ((c (char line i)))
        (cond
          ;; `- item' / `+ item'. A `*' bullet is org's third spelling and is
          ;; not read here: at column 0 it is a headline, and telling the two
          ;; apart by indentation is a rule this file would be the only reader
          ;; of.
          ((and (member c '(#\- #\+))
                (< (1+ i) n)
                (%org-blank-p (char line (1+ i))))
           (list i (or (cdr (assoc c *org-modern-list-bullets*)) "-") (+ i 2)))
          ((digit-char-p c)
           (let ((k i))
             (loop while (and (< k n) (digit-char-p (char line k))) do (incf k))
             (when (and (< (1+ k) n)
                        (member (char line k) '(#\. #\)))
                        (%org-blank-p (char line (1+ k))))
               (list i
                     (concatenate 'string (subseq line i k) ".")
                     (+ k 2))))))))))

(defun %org-frozen-figure (line)
  "The path of the figure LINE draws, or NIL.

`%org-image-links' rule asked of one line instead of the buffer: a link *alone
on its line* is a figure and a link with prose around it is a reference. That
function answers character offsets into the buffer, which is what an overlay
needed and what a builder working line by line has no use for."
  (let ((i (position-if-not #'%org-blank-p line)))
    (when i
      (let ((j (%org-link-at line i)))
        (when (and j (every #'%org-blank-p (subseq line j)))
          (let ((parts (%org-link-parts (subseq line i j))))
            (and parts (%org-image-target (car parts)))))))))

(defun %org-frozen-groups (v roles blocks &optional display)
  "The line vector V grouped into the things a page is made of.

A list of (KIND FIRST LAST . EXTRA), in document order:

  (:TITLE i i)      (:SUBTITLE i i)   (:RULE i i)
  (:HEADING i i)
  (:PARA a b)       one or more prose lines with no blank between them
  (:LIST a b)       a run of list items, continuations included
  (:TABLE a b)      a run of `|' rows
  (:FIGURE i i P)   a link alone on its line naming a drawable file
  (:LATEX a b SRC)  a display equation, owning its lines whole
  (:BLOCK a b NAME LANG)   a block's *body*, named from the line it opened on

DISPLAY is what `%org-frozen-latex-map' answered: the display equations, which
claim their lines the way a block claims its body, so the paragraph either side
of `\\[ ... \\]' is two paragraphs and not one with a hole in it.

Machinery is absent rather than marked. That is the deletion this port is
mostly made of: hiding a drawer used to mean anchoring a `fold' overlay on the
end of the previous line, with a special case for line 0 — which no fold can
reach — and merging consecutive runs so the renderer resolved fewer of them per
frame. None of it is a page's business, and none of it survives."
  (let* ((n (length roles))
         (opens (make-array n :initial-element nil))
         (maths (make-array n :initial-element nil))
         (out nil)
         (i 0))
    ;; A block is emitted where its `#+begin_' is, so the page keeps document
    ;; order without the walk below having to look behind itself.
    (dolist (b blocks)
      (let ((at (max 0 (1- (third b)))))
        (when (< at n) (setf (aref opens at) b))))
    (dolist (d display)
      (when (< (first d) n) (setf (aref maths (first d)) d)))
    (loop while (< i n)
          do (let ((b (aref opens i))
                   (m (aref maths i))
                   (role (aref roles i))
                   ;; Decoded, because the three questions asked of it below are
                   ;; asked of *characters*: a figure's path is handed to a
                   ;; primitive that will encode it again, and a list marker's
                   ;; indent is a column count.
                   (line (%org-frozen-line v i)))
               (cond
                 (b (destructuring-bind (name lang from to) b
                      (push (list :block from to name lang) out)
                      ;; Past the body and past the `#+end_' after it. The same
                      ;; arithmetic covers an empty block, whose FIRST is one
                      ;; past its LAST, and an unterminated one, whose LAST is
                      ;; the last line there is.
                      (setf i (min n (+ to 2)))))
                 ((member role '(:hidden :block)) (incf i))
                 ;; Before the prose arms and after the machinery ones: a
                 ;; display equation is prose that has stopped being prose, and
                 ;; `latex-fragments' has already declined to look inside a
                 ;; block, so a role of NIL is the only place one can land.
                 ((and m (null role))
                  (push (list :latex (first m) (second m) (third m)) out)
                  (setf i (min n (1+ (second m)))))
                 ((eq role :heading) (push (list :heading i i) out) (incf i))
                 ((eq role :title) (push (list :title i i) out) (incf i))
                 ((eq role :subtitle) (push (list :subtitle i i) out) (incf i))
                 ((eq role :rule) (push (list :rule i i) out) (incf i))
                 ((eq role :table)
                  (let ((a i))
                    (loop while (and (< i n) (eq (aref roles i) :table)) do (incf i))
                    (push (list :table a (1- i)) out)))
                 ((%org-frozen-blank-line-p line) (incf i))
                 ((%org-frozen-figure line)
                  (push (list :figure i i (%org-frozen-figure line)) out)
                  (incf i))
                 ((%org-frozen-list-item line)
                  (let ((a i))
                    (incf i)
                    ;; A list runs on through indented continuation lines and
                    ;; through further markers, and stops at a blank line or at
                    ;; unindented prose — which is org's own rule, near enough
                    ;; for a renderer and much shorter than org's own.
                    (loop while (and (< i n)
                                     (null (aref roles i))
                                     (null (aref maths i))
                                     (let ((l (%org-frozen-line v i)))
                                       (and (not (%org-frozen-blank-line-p l))
                                            (or (%org-frozen-list-item l)
                                                (%org-blank-p (char l 0))))))
                          do (incf i))
                    (push (list :list a (1- i)) out)))
                 (t
                  (let ((a i))
                    (loop while (and (< i n)
                                     (null (aref roles i))
                                     (null (aref maths i))
                                     (let ((l (%org-frozen-line v i)))
                                       (and (not (%org-frozen-blank-line-p l))
                                            (not (%org-frozen-figure l))
                                            (or (= i a)
                                                (not (%org-frozen-list-item l))))))
                          do (incf i))
                    (push (list :para a (1- i)) out))))))
    (nreverse out)))

;;; ---------------------------------------------------------------------------
;;; The hook a mode built on top of this one decorates the page through

(defvar *org-frozen-node-functions* nil
  "Functions called as each heading's node is built, to decorate it.

*The policy hook for anything a document means that org does not*, and the
register is `*org-mode-functions*' and `*org-modern-appear-inhibit-modes*': a
list this file walks, pushed onto by the mode that wants it, so a curriculum
becomes clickable without a line here knowing what a curriculum is.

**Called with one argument, a plist**, and answering **a list of extra
arguments to splice into that heading's `block' form**, or NIL for no opinion.

The plist:

  :KIND    always :HEADING today. Present so a second kind of node can be
           hooked later without every caller having to be found and changed.
  :LEVEL   the heading's level — 1 for `*', 2 for `**'.
  :TEXT    its text, decoded to real characters, with the tag run removed and
           the surrounding whitespace trimmed. Characters and not bytes: see
           `%org-frozen-text', and do not undo it.
  :TAGS    its `:a:b:' tags as a list of strings, or NIL.
  :LINE    the 0-based index of the line in the buffer.
  :BEGIN   character offset of the line's first character, which is what
           `goto-char', `replace-region' and `%org-property' all take.
  :END     character offset just past its last.

The answer is appended to the `block' call after the heading's own text node, so
anything `block' takes is allowed and interleaving is fine — `:tag' to make the
whole heading clickable (padding included), `:background' or `:border' to turn it
into a card, `:pad' and `:gap' to give it room, and further nodes built with
`text', `block', `rect' or `image' to put a status pill or a button beside it.

  (defun my-decorate (h)
    (when (equal (getf h :kind) :heading)
      (list :tag (lambda () (message \"clicked\"))
            (text (run \"done\" :face \"link\")))))
  (pushnew 'my-decorate *org-frozen-node-functions*)

Each function is called inside IGNORE-ERRORS, as `*org-mode-functions*' is and
for the same reason: one config's broken hook must not stop the next one from
running, and must not turn opening a `.org' file into a backtrace. An answer
that is not a list is ignored.

DEFVAR and not DEFPARAMETER, so reloading this file does not forget who is on
it.")

(defun %org-frozen-decorate (context)
  "Everything `*org-frozen-node-functions*' wants added, in one list."
  (let ((out nil))
    (dolist (f *org-frozen-node-functions* out)
      (let ((extra (ignore-errors (funcall f context))))
        (when (and extra (listp extra))
          (setf out (append out extra)))))))

;;; ---------------------------------------------------------------------------
;;; Building the page
;;;
;;; One function per kind of group, each answering a *list* of nodes so that a
;;; group may contribute the space above itself as well as itself. They are
;;; ordinary calls to the builders in `runtime/gui.lisp', which answer printed
;;; source; nothing here holds a handle to anything, because a scene is described
;;; in one form and swapped in whole.

(defun %org-frozen-line (v i)
  "Line I of V, decoded, with its indentation left alone.

The one place buffer text becomes characters, and every reader below this point
depends on it having happened exactly once."
  (%org-frozen-text (first (aref v i))))

(defun %org-frozen-line-pieces (text frags &optional (from 0) (to (length text)))
  "TEXT between FROM and TO as pieces, with FRAGS spliced in as equations.

FRAGS is ((BEG END SOURCE) ...) in indices into TEXT and in order — the entry
`%org-frozen-latex-map' made for this line. Each one replaces its own
characters, so the source of an equation is never on the page beside the
equation.

The stretches either side are handed to `%org-frozen-pieces' one at a time,
which means org's inline scanner sees the text *around* an equation rather than
through it — and that is the right answer as well as the easy one: a `$' is not
an emphasis marker, but a `_' inside one is, and `$a_1$ and $b_1$' would
otherwise read as one italic run from the first subscript to the second."
  (let ((out nil) (i from))
    (dolist (f frags)
      (destructuring-bind (a b source) f
        (when (and (<= from a) (<= b to) (>= a i))
          (when (< i a)
            (setf out (revappend (%org-frozen-pieces (subseq text i a)) out)))
          (push (%org-frozen-latex-piece source) out)
          (setf i b))))
    (when (< i to)
      (setf out (revappend (%org-frozen-pieces (subseq text i to)) out)))
    (nreverse out)))

(defun %org-frozen-chunk (v i &optional (from 0) to)
  "Line I of V between FROM and TO as pieces, trimmed, equations and all.

NIL for a line with nothing on it, which is what a paragraph builder reads as a
blank line.

Trimmed by moving FROM and TO rather than by trimming the string, because a
fragment's indices are into the line as it stands and `string-trim' would slide
every one of them."
  (let* ((text (%org-frozen-line v i))
         (n (length text))
         (to (min (or to n) n))
         (from (min from to))
         (a (or (position-if-not #'%org-blank-p text :start from :end to) to))
         (b (let ((e to))
              (loop while (and (> e a) (%org-blank-p (char text (1- e)))) do (decf e))
              e)))
    (when (< a b)
      (%org-frozen-line-pieces text (and *org-frozen-inline*
                                         (< i (length *org-frozen-inline*))
                                         (aref *org-frozen-inline* i))
                               a b))))

(defun %org-frozen-plain-chunk (text)
  "TEXT as pieces, with no equations in it.

For the two places a line is *transformed* before it is drawn — a block body
with org's escaping comma taken off — where the fragment indices no longer
describe the string. Neither can hold an equation anyway: `latex-fragments'
treats every `#+begin_'/`#+end_' body as opaque."
  (let ((s (string-trim '(#\Space #\Tab #\Return) text)))
    (when (plusp (length s)) (%org-frozen-pieces s))))

(defun %org-frozen-paragraph (chunks &optional style)
  "CHUNKS — one list of pieces per source line — as one `text', or NIL when
there is nothing in them.

A paragraph is a *flow*: the lines are joined and the font decides where the
breaks fall, which is the whole difference from a grid, where a break is
wherever column 80 was. They are joined with a space rather than concatenated
because a line ending in `the' and one beginning `same' are two words.

*Chunks* and not strings, and the reason is the equations. Each line has already
been cut into pieces by `%org-frozen-chunk', which is where a `$x_1$' became an
image and where the indices that said so were still valid; joining strings first
and scanning afterwards would have thrown those indices away one step before
they were needed.

Scanned per line rather than over the join for a second reason, which predates
the maths: org's emphasis rule is line-local — `%org-modern-line' applies it
that way and `crates/syntax' agrees — so joining first would let a `*' at the
end of one line pair with a `*' on the next and embolden the gap between two
sentences.

The joining space goes on the *end of the previous piece*, which is one fewer
run than a space of its own. An equation cannot carry one, having no string to
put it on, so a line ending in maths gets the space as its own piece."
  (let ((pieces nil))
    (dolist (c chunks)
      (when c
        (when pieces
          (let ((tail (car (last pieces))))
            (if (eq (car tail) :image)
                (setf pieces (append pieces (list (cons " " nil))))
                (setf (car tail) (concatenate 'string (car tail) " ")))))
        (setf pieces (append pieces c))))
    (let ((runs (%org-frozen-runs pieces style)))
      (when runs (apply #'text runs)))))

(defun %org-frozen-prose (chunks &optional style)
  "CHUNKS as one `text' per paragraph, an empty chunk splitting them.

For the places prose arrives in bulk — a quote block, and nothing else today."
  (let ((out nil) (acc nil))
    (dolist (c chunks)
      (cond ((null c)
             (let ((p (%org-frozen-paragraph (nreverse acc) style)))
               (when p (push p out)))
             (setf acc nil))
            (t (push c acc))))
    (let ((p (%org-frozen-paragraph (nreverse acc) style)))
      (when p (push p out)))
    (nreverse out)))

(defun %org-frozen-heading-size (level)
  "The `:size' of a level-LEVEL heading, as a percentage of the body.

`*org-modern-heading-scale*' is org-modern's list and is read rather than
copied, so a config that resizes its headings resizes them in both places. A
level past the end of the list is body size, which is what stops a deep outline
from being a wall of large type."
  (round (* 100 (or (nth (1- level) *org-modern-heading-scale*) 1))))

(defun %org-frozen-heading-node (v i)
  "The nodes a heading on line I contributes: the space above it, and itself.

The size lives on the *runs* and not on the line, which is the third thing the
grid could not do. `Overlay::scale' is a line property wherever it lands — the
widest scale claimed by any overlay touching a line sets that line's cell — so
org-modern has to put `scale' on the *stars* to size the heading, and then a
separate overlay on the text to embolden it, because weight is per-run and size
is not. Here both are keywords on a run, the bullet is set smaller than the words
it announces, and the emphasis inside a heading is drawn instead of skipped."
  (destructuring-bind (line begin end) (aref v i)
    (declare (ignore line))
    (let* ((text-line (%org-frozen-line v i))
           (level (or (%org-line-level text-line) 1))
           (at (%org-frozen-tags text-line))
           (body (string-trim '(#\Space #\Tab #\Return)
                              (subseq text-line level (or at (length text-line)))))
           (tags (when at (%org-frozen-tag-list (subseq text-line at))))
           (size (%org-frozen-heading-size level))
           (face (nth (min (1- level) (1- (length *org-frozen-heading-faces*)))
                      *org-frozen-heading-faces*))
           (bullet (nth (mod (1- level) (length *org-modern-stars*))
                        *org-modern-stars*))
           ;; Through the chunk builder rather than straight to
           ;; `%org-frozen-pieces', so that `** Compute $P_M f$' is a heading
           ;; with an equation in it. It costs nothing: the same function reads
           ;; the same line, from the stars to the tags.
           (head (apply #'text
                        (cons (run (concatenate 'string bullet " ")
                                   :size (max 1 (round (* size 0.75)))
                                   :family :mono
                                   :face "markup")
                              (%org-frozen-runs
                               (%org-frozen-chunk v i level at)
                               (list :size size :bold t :face face))))))
      ;; A tag is a filing decision and not something the page says, so it is
      ;; not drawn — the same call the grid version made, for the same reason,
      ;; and now without the `display' of a single space that was the best a
      ;; substitution could manage. It reaches the hook instead, which is where
      ;; a mode that means something by `:paused:' can say so.
      (list (rect :height (%org-frozen-px *org-frozen-heading-space*))
            (apply #'block
                   :width (pct 100)
                   head
                   (%org-frozen-decorate
                    (list :kind :heading :level level :text body :tags tags
                          :line i :begin begin :end end)))))))

;;; An org table is aligned by *org*, in the buffer, by rewriting the text — and
;;; this mode never rewrites anything, so alignment here has to be a drawing.
;;; The two readers below are the same two the grid version had and are not
;;; changed by the port: a row's cells, and whether a row is a rule.
;;;
;;; What *is* drawn changed completely. The grid version put one `display'
;;; overlay per row holding the whole row re-padded with spaces, which is what
;;; made `| *bold* |' come out as six literal characters — the row was one flat
;;; string and there was nowhere inside it for a face to go. Here a cell is a
;;; box holding a paragraph, so it is emphasised, wrapped and coloured like any
;;; other paragraph, and the padding is *layout* rather than spaces.
;;;
;;; ponytail: a `|' inside a cell splits it. Org escapes an embedded bar as
;;; `\\vert' and nothing here reads that. Unchanged from the grid version, and
;;; the other half of that note — that a column's width is measured in
;;; *characters*, so a column of CJK asks for less room than it needs — is now
;;; `*org-frozen-table-pad*''s, where the whole approximation lives.

(defun %org-frozen-cells (line)
  "The `|'-delimited cells of a table row, as decoded strings with their padding
already trimmed off.

Trimmed and decoded here rather than by the caller because *every* caller wants
both: a column's width is the width of its content in characters, and the
content is what gets redrawn. Whatever whitespace the author used to align it by
hand is exactly the thing this mode is replacing.

Decoding here is also why `%org-frozen-table-node' is the one builder that reads
a *raw* line: handing this an already-decoded one would decode an em dash twice."
  (let ((bars (loop for i from 0 below (length line)
                    when (char= (char line i) #\|) collect i)))
    (loop for (a b) on bars
          while b
          collect (let ((s (1+ a)) (e b))
                    (loop while (and (< s e) (%org-blank-p (char line s))) do (incf s))
                    (loop while (and (< s e) (%org-blank-p (char line (1- e)))) do (decf e))
                    (%org-frozen-text (subseq line s e))))))

(defun %org-frozen-rule-row-p (line)
  "True when LINE is a table's rule row — `|---+---|' and its spellings.

Tested on the whole line rather than per cell, because that is the whole of
org's rule: a row made of bars, dashes, pluses and space, with at least one
dash in it. The last clause is what stops `| | |' — an empty row — from being
drawn as a rule."
  (and (find #\- line)
       (every (lambda (c) (member c '(#\| #\- #\+ #\Space #\Tab #\Return))) line)))

(defun %org-frozen-table-node (v from to)
  "Lines FROM..TO of V, which are consecutive `|' rows, as one table.

A `Block' of row `Block's, each cell a box of a **stated width**, and every row
stating the *same* widths — which is the whole of what `docs/gui.org' says about
tables: a column's width is a constraint across siblings, no per-child `Length'
can express it, and `:auto' would size each cell to its own content so column
one would be as wide as `Name' in one row and `Wolfgang' in the next.

The widths are **percentages**, computed here once from the widest cell of each
column — the same arithmetic the grid version did in characters, answered as a
share of the measure rather than as a pixel count, because a pixel count is a
guess about a font and this is not. See `*org-frozen-table-pad*'. And measured
on what the cell *draws* rather than on what it says, which is new: a cell
reading `*bold*' is four characters wide on the page and six in the file.

The rule row is a `rect', not a run of `─'. The header is everything above the
first rule, and only when there is one: a table with no rule has no header, it
has rows — and the header is *weight*, not colour, because the rule under it
already says where it ends."
  ;; The *raw* line, uniquely in this file: `%org-frozen-cells' decodes each
  ;; cell for itself — it has to, since a column's width is its content's width
  ;; in characters — and handing it a line this builder had already decoded
  ;; would decode an em dash twice and read its own output as UTF-8.
  (let* ((rows (loop for i from from to to collect (first (aref v i))))
         (cellsets (mapcar (lambda (r) (mapcar #'%org-frozen-pieces
                                               (%org-frozen-cells r)))
                           rows))
         (rules (mapcar #'%org-frozen-rule-row-p rows))
         (ncols (reduce #'max (mapcar #'length cellsets) :initial-value 0))
         (chars (make-list ncols :initial-element 0))
         (header (position t rules)))
    (when (plusp ncols)
      ;; Widths come from the rows that carry content. A rule row's cells are
      ;; runs of dashes as long as the author felt like making them, and letting
      ;; one set a column's width would make the table as wide as its own
      ;; punctuation.
      (loop for cs in cellsets for rule in rules
            unless rule
              do (loop for c in cs for j from 0
                       do (setf (nth j chars)
                                (max (nth j chars)
                                     (length (%org-frozen-drawn c))))))
      (let* ((total (reduce #'+ chars
                            :key (lambda (c) (+ c *org-frozen-table-pad*))
                            :initial-value 0))
             ;; The last column takes whatever the rounding left over, so the
             ;; shares sum to exactly 100 and the table ends where the measure
             ;; does rather than a pixel or two short of it.
             (shares (let ((out (loop for c in chars
                                      collect (max 1 (round (* 100 (+ c *org-frozen-table-pad*))
                                                            (max 1 total))))))
                       (setf (nth (1- ncols) out)
                             (max 1 (- 100 (reduce #'+ (butlast out) :initial-value 0))))
                       out)))
        (list
         (apply #'block
                :dir :column :gap (%org-frozen-px *org-frozen-table-row-gap*)
                :width (pct 100)
                (loop for cs in cellsets
                      for rule in rules
                      for i from 0
                      collect
                      (if rule
                          (rect :width (pct 100) :height (%org-frozen-px 1)
                                :fill "markup")
                          (apply #'block
                                 :dir :row :width (pct 100)
                                 (loop for j from 0 below ncols
                                       collect
                                       (let ((runs (%org-frozen-runs
                                                    (nth j cs)
                                                    (when (and header (< i header))
                                                      (list :bold t)))))
                                         (block :width (pct (nth j shares))
                                                (when runs
                                                  (apply #'text runs))))))))))))))

(defun %org-frozen-uncomma (line)
  "LINE with org's escaping comma taken off, if it has one.

Org's rule for a block that quotes org: a line that would otherwise close the
block, or start a heading inside it, is written with a leading comma. A printed
page shows what was meant, so the comma goes.

Reshaped rather than deleted, and what went is the *workaround*. The grid
version had to cover two characters and draw one — `,#' replaced by `#' — because
a `display' of the empty string clears the property instead of hiding anything,
so the substitution needed the character after the comma to have something to
be. Building a run from a string that never had the comma in it is the whole of
it now. `docs/curriculum.org' is a document made almost entirely of this, and
the comma has to go before the syntax highlighter sees the block or every span
after it lands one character to the right."
  (let ((j (position-if-not #'%org-blank-p line)))
    (if (and j
             (< (1+ j) (length line))
             (char= (char line j) #\,)
             (member (char line (1+ j)) '(#\# #\*)))
        (concatenate 'string (subseq line 0 j) (subseq line (1+ j)))
        line)))

(defun %org-frozen-highlight (lang text)
  "The syntax runs of TEXT set in LANG, as ((BEG END FACE) ...) in character
offsets, or NIL for a language nothing knows.

A buffer has one language. Org has no tree-sitter grammar at all — it is
hand-scanned in `crates/syntax/src/org.rs' — so the highlighting thread that
colours every other buffer has nothing to say about a `#+begin_src python', and
could not say it in Python even if it did. `tree-sitter-highlight' has an
injection mechanism for precisely this and it is deliberately switched off, which
would in any case not help a language that has no grammar to inject from.

So the block's body is highlighted the only way it can be: by asking for it.
`(highlight LANG TEXT)' is `zemacs_syntax::highlight' with a Lisp face on it — a
**pure function** taking two strings and answering spans, touching no editor and
taking no lock.

Handed the whole block at once, because `highlight' parses whatever it is given:
asking per line would reparse the block once per line and would also get it
*wrong*, since a Python triple-quoted string or a Rust block comment is not a
line-local fact. The block is then cut back into lines afterwards, which is what
`docs/gui.org' asks for anyway — *a code block is one `text' node per line*,
because a paragraph is a flow and there are no hard breaks in one.

**The ceiling this used to carry is closed by the port.** It read: /one overlay
per token run, and a long listing has a few hundred; `Overlays' is a `Vec'
scanned linearly per drawn line, and the upgrade path is an interval tree./ A
span is a `run' inside its line's `text' now and **no overlay is made at all**,
so there is nothing to scan per drawn line and nothing to be linear in. The
interval tree is still owed — to avy, which makes overlays — but it is not owed
to this file."
  (when lang (highlight lang text)))

(defun %org-frozen-code-pieces (line at end spans)
  "The pieces of one line of a listing, or NIL when the line is empty.

LINE occupies AT..END of the block's own text, and SPANS are the block's syntax
runs in that same space, so this is a clip and two subtractions.

*Pieces* and not runs, so that a line of code goes through `%org-frozen-runs'
like everything else on the page and gets the same coalescing: a listing has
more run boundaries per line than any prose does — one per token — and the
highlighter emits neighbouring spans of the same face often enough that this is
worth having.

A blank line answers one space, which is a paragraph of one line and so of one
line's height. That used to be a `rect' of a height computed from the body's
point size, because a paragraph whose only content was whitespace laid out to
*no lines at all* — the wrapper parks whitespace as pending and flushes it only
in front of a word — so a blank line between two functions closed up and the
listing lost its paragraphing. `crates/gui' flushes a paragraph that never met a
word now, which is where the rule belongs, and this file stopped needing to know
how tall a line is."
  (let ((out nil) (i at))
    (dolist (s spans)
      (destructuring-bind (a b face) s
        (let ((a (max a at)) (b (min b end)))
          (when (< a b)
            (when (< i a) (push (cons (subseq line (- i at) (- a at)) nil) out))
            (push (cons (subseq line (- a at) (- b at)) (list :face face)) out)
            (setf i b)))))
    (when (< i end) (push (cons (subseq line (- i at) (- end at)) nil) out))
    (or (nreverse out) (list (cons " " nil)))))

(defun %org-frozen-block-node (v from to name lang)
  "The nodes a `#+begin_NAME' body contributes, or NIL when it contributes none.

Two shapes and the difference between them is the whole of it. A *listing* is a
tinted panel with a gutter of padding: `:background' on a `block' paints the
whole box, which is what `line-background' had to exist to do on the grid —
an overlay `background' paints the cells a range covers, so a block drawn with
one was a stripe as ragged as its own code. A *quote* is inset on every side —
a real narrower measure, not the same width pushed right — and set in the
comment face, so it recedes.

An empty body leaves no trace at all, which is right for a block with nothing in
it, and a hidden one leaves none by not being built."
  (let ((look (%org-frozen-block name)))
    (when (and (<= from to) (not (getf look :hide)))
      (let ((lines (loop for i from from to to
                         collect (%org-frozen-uncomma (%org-frozen-line v i)))))
        (cond
          ((getf look :quote)
           ;; Inset on every side and set in the comment face, italic — which is
           ;; what a quotation is set in and what this arm briefly was not.
           ;;
           ;; It was not, for a while, because an italic run measured narrower
           ;; than it drew: `advance' summed the whitespace-delimited pieces of a
           ;; string and the painter drew the string, and a face whose glyphs are
           ;; sheared is exactly where those two numbers part company. Inside one
           ;; run it never showed; at the boundary between two it showed in full,
           ;; and an italic quote joined its source lines as `sidesrather', one
           ;; glyph overlapping the next. `crates/render' measures per character
           ;; now, so what layout records and what the painter draws are the same
           ;; number, and the italic is back where it belongs.
           (list (apply #'block
                        :width (pct 100)
                        :pad (%org-frozen-px *org-frozen-quote-pad*)
                        :gap (%org-frozen-px *org-frozen-gap*)
                        (%org-frozen-prose
                         (mapcar #'%org-frozen-plain-chunk lines)
                         (list :italic t :face "comment")))))
          (t
           (let* ((spans (when (getf look :highlight)
                           (%org-frozen-highlight (%org-frozen-language lang)
                                                  (format nil "~{~a~^~%~}" lines))))
                  (at 0)
                  (nodes nil))
             (dolist (l lines)
               (let* ((end (+ at (length l)))
                      ;; `:family :mono' is not a preference. A scene sets prose
                      ;; in a proportional face by default — that is most of why
                      ;; it exists — and a column of code lines up only if every
                      ;; character is the same width. A `#+begin_src' body in the
                      ;; body face is a listing whose indentation means nothing.
                      (runs (%org-frozen-runs
                             (%org-frozen-code-pieces l at end spans)
                             (list :family :mono))))
                 (push (apply #'text runs) nodes)
                 ;; ...and one for the newline that joined them.
                 (setf at (1+ end))))
             (list (apply #'block
                          :width (pct 100)
                          :pad (%org-frozen-px *org-frozen-code-pad*)
                          :background (when (getf look :band) *org-frozen-code-face*)
                          (nreverse nodes))))))))))

(defun %org-frozen-list-node (v from to)
  "Lines FROM..TO of V as one list, each item a row of marker and body.

**The hanging indent**, which is what a scene buys and a grid could not.
`Overlay::line_prefix' is documented as drawn in the left margin of *every* row
of every line the overlay touches — first row included — which is exactly right
for a source block and exactly wrong for a list item, where the first row carries
the bullet and the continuations have to align with the *text*. So a wrapped
item's second line used to start at column 0, and nobody had complained only
because `text-width' 80 in a wide pane means list items rarely wrap.

Here the marker and the body are two children of a row: the marker is a box as
wide as the marker and the body is a box of its own, so the body's *whole*
paragraph — every wrapped line of it — starts where the text starts. The body
has to be wrapped in a block rather than being a bare `text', because
`crates/gui' gives a paragraph its parent's full measure down a column and its
own natural width across a row.

A checkbox is drawn with org-modern's glyph and is deliberately not clickable
yet: writing a cookie back means `replace-region' into a read-only buffer, which
is `with-inhibited-read-only' and a decision about who owns the document, and
`*org-frozen-node-functions*' is where a mode that has made that decision joins
in."
  (let ((items nil) (start nil))
    ;; Split the run into items first — an item is a marker line and everything
    ;; indented under it — so that the row builder below never has to look
    ;; ahead.
    (loop for i from from to to
          do (when (%org-frozen-list-item (%org-frozen-line v i))
               (when start (push (cons start (1- i)) items))
               (setf start i)))
    (when start (push (cons start to) items))
    (list
     (apply #'block
            :dir :column :gap (%org-frozen-px *org-frozen-list-gap*) :width (pct 100)
            (loop for (a . b) in (nreverse items)
                  collect
                  (let ((line (%org-frozen-line v a)))
                    (destructuring-bind (indent bullet body)
                        (%org-frozen-list-item line)
                      (let* ((box (%org-checkbox-at line body))
                             (glyph (when box
                                      (cdr (assoc (char line (1+ (car box)))
                                                  *org-modern-checkboxes*))))
                             ;; The cookie's glyph goes into the item's own
                             ;; *text*, not beside it: `[ ] Do the thing' is one
                             ;; sentence, and a checkbox in a box of its own
                             ;; would put the words on the next line. A piece in
                             ;; front of the line's own rather than a string
                             ;; spliced into it, because splicing would slide
                             ;; every equation index on the line — `- [ ] show
                             ;; $x > 0$' is an ordinary thing to write.
                             (head (let ((rest (%org-frozen-chunk
                                                v a (if glyph (cdr box) body))))
                                     (if glyph
                                         (cons (cons (concatenate 'string glyph " ")
                                                     (list :family :mono))
                                               rest)
                                         rest)))
                             (para (%org-frozen-paragraph
                                    (cons head
                                          (loop for k from (1+ a) to b
                                                collect (%org-frozen-chunk v k))))))
                        (block :dir :row :width (pct 100)
                               :gap (%org-frozen-px *org-frozen-marker-gap*)
                               ;; The marker's box is `auto' — as wide as the
                               ;; marker — because the alternative is a stated
                               ;; width in device pixels, and a `1.' that did
                               ;; not fit one would wrap onto two lines inside
                               ;; its own column. A nested item is indented with
                               ;; *spaces in front of the marker*, measured by
                               ;; the font: the depth is whitespace in the file
                               ;; and turning it into structure would mean
                               ;; deciding what an inconsistently indented list
                               ;; means.
                               (block (text (run (concatenate
                                                  'string
                                                  (make-string indent
                                                               :initial-element #\Space)
                                                  bullet)
                                                 :family :mono
                                                 :face "markup")))
                               (block :width 'fill para))))))))))

(defun %org-frozen-figure-node (path)
  "PATH drawn as a figure, centred, or NIL when there were no pixels.

The same primitive an overlay figure uses — `image-file', answering a bare
`ImageId' and hanging nothing — so a figure a scene shows and a figure an
overlay shows are one bitmap and one cache entry. Core fills the node's size in
from its own image table at install time, so `(image ID)' is the whole form.

`*org-image-width*' is in **ems**, which is org-modern's parameter and its
reasoning: the device pixel is something only the renderer knows, so a width
computed here from `(font-size)' would come out half-size on a Retina display."
  (let ((id (ignore-errors (image-file (%org-expand-file path) *org-image-width*))))
    (when id
      (block :width (pct 100) :align :center (image id)))))

;;; ---------------------------------------------------------------------------
;;; `%org-frozen-latex' used to live here.
;;;
;;; It was the same preview pass `init.lisp' runs, with the "skip the fragment
;;; point is inside" rule taken out, because nothing is typed here and that rule
;;; has nothing to protect. It made `image' **overlays**, and a buffer showing a
;;; scene draws no overlays at all — so it drew nothing, and the maths moved up
;;; the file to `%org-frozen-latex-map', where it is part of the document model
;;; rather than a pass over it.
;;;
;;; The passes it left behind are still worth having and still run, because this
;;; mode derives from `org-mode' and org-mode's body goes first:
;;; `org-latex-preview-new' typesets every fragment on the way in, which warms
;;; exactly the cache `latex-preview' answers this file's questions from, and
;;; leaves the manuscript underneath already previewed for the moment you press
;;; `SPC m z' and go back to it.

;;; ---------------------------------------------------------------------------
;;; The commands

(defun %org-frozen-nodes (v groups)
  "Every group of GROUPS as the nodes it draws, in one flat list."
  (let ((out nil))
    (dolist (g groups (nreverse out))
      (destructuring-bind (kind from to &rest extra) g
        (dolist (node
                 (case kind
                   (:title
                    (let ((value (%org-frozen-keyword (%org-frozen-line v from)
                                                      "TITLE")))
                      (when (plusp (length value))
                        ;; Centred, which is the fourth thing the grid could not
                        ;; do: an overlay has no notion of where a line's middle
                        ;; is, and `align' on a `text' moves its lines within
                        ;; the paragraph's own width.
                        (list (text :align :center
                                    (run value
                                         :size (round (* 100 (or *org-frozen-title-scale* 1)))
                                         :bold t))))))
                   (:subtitle
                    (let ((value (%org-frozen-keyword (%org-frozen-line v from)
                                                      "SUBTITLE")))
                      (when (plusp (length value))
                        (list (text :align :center
                                    (run value
                                         :size (round (* 100 (or *org-frozen-subtitle-scale* 1)))
                                         :italic t :face "comment"))))))
                   ;; A rule is chrome and takes the markup face, which is the
                   ;; face org's own delimiters are drawn in — so a theme that
                   ;; dims its punctuation dims this too.
                   (:rule
                    (list (rect :width (pct *org-frozen-rule-width*)
                                :height (%org-frozen-px 1) :fill "markup")))
                   (:heading (%org-frozen-heading-node v from))
                   (:para
                    (let ((p (%org-frozen-paragraph
                              (loop for i from from to to
                                    collect (%org-frozen-chunk v i)))))
                      (when p (list p))))
                   (:latex (list (%org-frozen-latex-node (first extra))))
                   (:list (%org-frozen-list-node v from to))
                   (:table (%org-frozen-table-node v from to))
                   (:block (%org-frozen-block-node v from to
                                                   (first extra) (second extra)))
                   (:figure
                    ;; A figure whose file will not decode falls back to the
                    ;; path, which is the link you can read — the same trade
                    ;; `org-inline-images' makes, and better than a blank.
                    (list (or (%org-frozen-figure-node (first extra))
                              (text (run (first extra) :face "comment")))))
                   (t nil)))
          (when node (push node out)))))))

(defun %org-frozen-page (v groups)
  "The whole page: a centred measure with margins either side of it.

`Fill | Pct | Fill' across a row, which is the worked example `docs/gui.org'
gives for exactly this and the reason `Length' has a `Fill' at all. The two
spacers paint nothing — a `rect' with no `:fill' is a spacer, which is the only
reading under which `None' on a fill is usable."
  (block :dir :row :width (pct 100) :pad (%org-frozen-px *org-frozen-page-pad*)
         (rect :width 'fill)
         (apply #'block
                :dir :column :width (pct *org-frozen-measure*)
                :gap (%org-frozen-px *org-frozen-gap*) :align :center
                (%org-frozen-nodes v groups))
         (rect :width 'fill)))

(defun org-frozen-refresh ()
  "Redraw the page.

One scan, one grouping pass, one `highlight' per source block, and one
`scene-set' — which swaps the whole page in and keeps the reader's place, so a
document that re-renders because a program wrote a paragraph into it does not
throw somebody on page nine back to page one.

Safe to press as often as you like, and it needs no `clear' first: a scene is
described in one form and swapped in whole, so there is nothing of the last page
left to take off. That is one whole function this mode used to need
(`org-frozen-clear', which walked the buffer's overlays looking for its own
`:org-frozen' mark) and one whole class of bug it used to have."
  (let* ((v (%org-frozen-lines))
         (scan (%org-frozen-scan v)))
    (multiple-value-bind (inline display) (%org-frozen-latex-map v)
      ;; Bound around the whole build and nowhere else — see
      ;; `*org-frozen-inline*' for why it is a binding rather than an argument.
      (let* ((*org-frozen-inline* inline)
             (groups (%org-frozen-groups v (car scan) (cdr scan) display)))
        (scene-set (%org-frozen-page v groups))
        ;; Ends on the message so the form answers NIL: `eval-string' echoes the
        ;; value of the last form and would otherwise wipe out what this said.
        (message (format nil "~d node~:p, ~d block~:p, ~d equation~:p"
                         (length groups) (length (cdr scan))
                         (+ (length display)
                            (loop for i from 0 below (length inline)
                                  sum (length (aref inline i))))))))))

(defvar *org-frozen-buffer* nil
  "The file (or buffer name) currently frozen, or NIL.

DEFVAR rather than DEFPARAMETER, and it exists for one reason: `*major-mode*' in
`modes.lisp' is a single global, so `X-exit-hook' fires whenever the image
*enters* a different mode — including opening a `.rs' file in another window
while this document is still frozen in this one. The exit hook has to be able to
tell \"the user left frozen mode\" from \"the user opened something else\", and
the buffer's identity is the only thing that answers it.

The ceiling underneath is `modes.lisp''s own and is written up there: there is
no buffer-switch hook, so nothing here can be per-buffer in the way it should
be. Freeze two documents at once and the second one to be entered owns this.")

(defun org-frozen-mode-exit-hook ()
  "Leaving the mode puts the document back: the page comes down and the buffer
becomes editable again.

`(scene-set)' with no argument is how a page is taken down, and it gives the
buffer back the text that was under it the whole time — nothing was ever
rewritten to draw the page, which is the assertion worth having.

Guarded on the buffer, for the reason `*org-frozen-buffer*' gives. Taking the
read-only flag off the wrong buffer would be the dangerous half — a dired
listing quietly becoming writable is exactly the kind of bug that shows up as
data loss much later."
  (let ((me (or (buffer-file-name) (buffer-name))))
    (when (equal me *org-frozen-buffer*)
      (scene-set)
      (set-buffer-read-only nil)
      (setf *org-frozen-buffer* nil))))

(defun org-frozen-toggle ()
  "Switch between reading this org file and editing it."
  (if (derived-mode-p 'org-frozen-mode) (org-mode) (org-frozen-mode)))

;;; ---------------------------------------------------------------------------
;;; The mode
;;;
;;; Derived from `org-mode', which is the whole reuse story in one word: entering
;;; it runs org-mode's body first — org-modern on, LaTeX previewed, figures
;;; drawn, `*org-mode-functions*' run, so `math.lisp' still recognises a
;;; curriculum — and everything in the body below is what this mode adds *on
;;; top*. Nothing is copied and there is nothing to drift.
;;;
;;; Those inherited passes now make overlays a scene does not draw, and that is
;;; deliberate rather than an oversight: they are what the buffer looks like the
;;; moment you press `SPC m z' and get your manuscript back, and re-running them
;;; on the way out would be a buffer scan to restore a state that never went
;;; away.

(define-derived-mode org-frozen-mode org-mode
  ;; Read-only twice over, and both are real. The claim below is core's single
  ;; guard in `Editor::apply' — `i', `x', `p', `u', a paste and an `insert' from
  ;; Lisp are all refused by it, and `i' says so instead of parking you in a mode
  ;; where every keystroke bounces. Installing a scene claims it *again*, from
  ;; the other direction, because a scene is not an editing surface; the two
  ;; nest, and `set_scene' remembers what it found so taking the page down gives
  ;; back what was there.
  ;;
  ;; This is where a paragraph used to argue that everything which is *not* an
  ;; edit still works — every motion, `/', `n', the folds, `M-x' — and that this
  ;; is the difference between a read-only buffer and a disabled one. **A scene
  ;; takes selection, copy, `/' and `n' back**, and the argument was right, so
  ;; it is written down here rather than deleted:
  ;;
  ;;   ceiling: you cannot copy the theorem statement out of a frozen page, and
  ;;   you cannot search a 3000-line curriculum inside one. Both are real losses
  ;;   and neither is fixable from Lisp — there is no text under a scene to
  ;;   select, only boxes and runs. `docs/gui.org' lists them among its own
  ;;   ceilings and calls them the first thing to build after it renders.
  ;;
  ;;   the upgrade path, and it is short: a `Text' node's `Frame' already records
  ;;   every line it broke into and where each run sits on each line, so a drag
  ;;   across runs has all the geometry it needs and a find-in-page is a walk of
  ;;   the same runs. Both are `crates/gui' plus a mode key, and neither is a
  ;;   change to anything in this file.
  ;;
  ;; `SPC m z' is the honest answer meanwhile: it is one key back to a buffer
  ;; where all four work.
  (set-buffer-read-only t)
  (setf *org-frozen-buffer* (or (buffer-file-name) (buffer-name)))
  ;; org-modern is already on — org-mode's body saw to it — but a fragment may
  ;; be *revealed* under the cursor from before the mode changed. Nothing draws
  ;; it while the page is up; this is so that `SPC m z' hands back a manuscript
  ;; with its markup hidden rather than one with a hole in it.
  (when *org-modern-revealed*
    (%org-modern-rehide *org-modern-revealed*)
    (setf *org-modern-revealed* nil))
  (org-frozen-refresh))

;;; Reveal-on-cursor is the one org-modern behaviour a *renderer* must not have.
;;; It matters less than it did — a scene draws no overlays, so a revealed
;;; fragment is invisible either way — and it is kept because the buffer under
;;; the page is the buffer you get back, and getting back a manuscript with one
;;; fragment unwrapped because the cursor drifted while you were reading is
;;; exactly the surprise this list exists to prevent.
;;;
;;; Said as a list in org-modern rather than as a test for this mode, and pushed
;;; onto from here — the shape `*org-mode-functions*', `*after-change-functions*'
;;; and `*org-frozen-node-functions*' all have. org-modern does not know this
;;; file exists, and a second mode wanting the same thing is one more PUSHNEW.
(defvar *org-modern-appear-inhibit-modes* nil)
(pushnew "org-frozen-mode" *org-modern-appear-inhibit-modes* :test #'string=)

;;; The gutter is claimed in `init.lisp' beside org's, with the rest of the
;;; `set-no-gutter-modes' list. It buys nothing while a page is up — a scene has
;;; no rows to number — and it is what the buffer looks like on the way out.

;;; ---------------------------------------------------------------------------
;;; Redrawing after a write that was not yours
;;;
;;; A frozen buffer is read-only *to the keyboard* and is still written to — see
;;; `with-inhibited-read-only' in the shim, and the two callers this mode was
;;; built for: a transcribed handwritten solution landing in a problem's Response
;;; section, and a problem's `:ZEMACS_STATUS:' being toggled in place. Both are
;;; `replace-region', and both leave the page describing a document that has
;;; moved under it.
;;;
;;; The loop closes itself, and a click is the third caller: a `:tag' closure
;;; writes with `replace-region', the revision moves, this hook fires, the
;;; builder runs, and `scene-set' swaps the tree — carrying the scroll, so you
;;; are still looking at the problem you just answered. No new machinery, and the
;;; argument for redrawing here is *more* true than it was: nothing types into
;;; this one, and every change it will ever see is a program writing a paragraph.
;;;
;;; Two guards, cheapest first. `*org-frozen-buffer*' is a Lisp variable and free;
;;; `derived-mode-p' is a round trip through `%query' and is only reached once
;;; some buffer somewhere has actually been frozen. A config that never enters
;;; this mode pays one NIL test per keystroke.
(defun org-frozen-after-change ()
  (when (and *org-frozen-buffer* (derived-mode-p 'org-frozen-mode))
    (org-frozen-refresh))
  nil)

(defvar *after-change-functions* nil)
(pushnew 'org-frozen-after-change *after-change-functions*)

;;; ---------------------------------------------------------------------------
;;; Keys
;;;
;;; Fewer than there were, because two of them stopped meaning anything.
;;;
;;; `org-frozen-cycle' is gone. It was TAB: take this file's machinery folds off,
;;; let `org-cycle' see a buffer folded only by itself, and put them back —
;;; necessary because a fold is an overlay, `unfold-region' takes out every fold
;;; overlapping a subtree including the ones holding the drawers shut, and
;;; `%org-cycle-state' tells its three states apart by looking at the folds it
;;; finds. None of that exists here: there are no machinery folds because there
;;; is no hiding, and folding a line a scene is not drawing hides nothing.
;;;
;;; TAB is left inherited from `org-fold.lisp' rather than rebound, and pressing
;;; it in a frozen page folds text nobody is looking at and reports that it did.
;;; ponytail: harmless and useless. Folding *in a scene* cannot be an overlay —
;;; there is no line to hide — so it has to be Lisp state, a set of collapsed
;;; heading ids the builder consults, with a click on a heading toggling it. That
;;; is strictly simpler than what the old command did, and the place it would
;;; attach already exists: `%org-frozen-heading-node' takes a `:tag' from
;;; `*org-frozen-node-functions*'.

;;; `RET' follows the link under *point*, which a scene does not have. It is
;;; still bound because point is still somewhere and `SPC m z' is one key away —
;;; but the way to follow a link on a page is to **click it**, which is what
;;; `%org-frozen-markup' hangs a `:tag' on every link for.
(define-key "org-frozen-mode" "<ret>" "org-open-at-point")
(define-key "org-frozen-mode" "SPC m o" "org-open-at-point")

(define-key "org-frozen-mode" "SPC m m" "org-frozen-refresh")
(define-key "org-frozen-mode" "SPC m z" "org-frozen-toggle")
;;; ...and the way *in*, from an ordinary org buffer. One key, both directions.
(define-key "org-mode" "SPC m z" "org-frozen-toggle")
