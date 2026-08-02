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
;;;; instead. A property drawer is not dimmed, it stops occupying rows. A
;;;; `#+begin_src python' is not coloured as a directive, it is *gone*, and the
;;;; body under it is syntax-highlighted as Python inside a buffer whose own
;;;; language is org. A table is not one flat face, it is aligned columns under
;;;; a rule. That is the whole difference, and it is the reason the mode exists.
;;;;
;;;; It is *org-modern plus more*, not a second scanner. `org-frozen-mode'
;;;; derives from `org-mode', so entering it runs org-mode's own body first —
;;;; org-modern, the LaTeX previews, the inline figures — and everything below
;;;; is what it then puts *on top*. What org-modern needed for this is one hook
;;;; list and one clause reading it — `*org-modern-appear-inhibit-modes*', which
;;;; this file pushes its own name onto — because reveal-on-cursor is the single
;;;; org-modern behaviour a renderer must not have. org-modern does not know this
;;;; file exists, and a second reader mode is one more PUSHNEW.
;;;;
;;;; The three things it does not do, and why:
;;;;
;;;;   - **No heading numbering.** It reads well in a book and badly here: the
;;;;     documents this exists for — `docs/curriculum.org' and the tutor — write
;;;;     their own numbers into the Contents links (`[[id:unit-1][1. Vectors]]'),
;;;;     so generated numbers would sit beside hand-written ones that disagree
;;;;     the first time a unit is reordered. A document that wants them can put
;;;;     them in the text, where they are the same string the link uses.
;;;;   - **No captions.** `org-modern.lisp' already declines `#+CAPTION:' at
;;;;     length; a caption is a line of prose under a figure and already reads
;;;;     as one. Here the keyword line is simply hidden, so the prose is all you
;;;;     see, which is the right answer for the wrong reason and is fine.
;;;;   - **No export.** This renders the buffer you have open. Writing a PDF is
;;;;     a different program.
;;;;
;;;; ponytail — every overlay this file makes is a whole-buffer pass, redone by
;;;; `SPC m m'. There is no incremental path and there should not be: the
;;;; document is read-only, so the only things that can invalidate the drawing
;;;; are entering the mode, a fold cycling, and a theme change. Two of those are
;;;; keystrokes you press deliberately and the third is a redraw. The cost is
;;;; one `buffer-string', one `latex-fragments' and one `buffer-substring' per
;;;; source block, which is a curriculum-sized file in well under a frame.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; What a printed page looks like
;;;
;;; Eight parameters and one function, and between them they are the whole of
;;; this mode's taste. All DEFPARAMETER, so a config can set one and press
;;; `SPC m m' without reloading anything — the same contract `org-modern.lisp'
;;; offers for its bullets.

(defparameter *org-frozen-code-face* "modeline"
  "Face whose *colour* is the band behind a source block.

A face name and not an RGB triple, because that is the only colour vocabulary
an overlay has — see the ponytail note at the head of `crates/core/src/overlay.rs'.
`modeline' is borrowed rather than earned: there is no `code-background' face,
and the modeline's is the one colour in the theme that is already chosen to sit
*behind* text at low contrast, which is exactly the job. The cost is that a
config restyling its modeline restyles this band with it.

Ceiling: a listing that wants its own colour. The upgrade path is one line —
`(set-syntax-color \"modeline-inactive\" r g b)' on a face nothing else uses,
and this parameter pointed at it — or the RGB overlay property `overlay.rs'
already names as its own upgrade path.")

(defparameter *org-frozen-code-prefix* "  "
  "Drawn in the left margin of every line of a source block, with the code
pushed right by exactly its width.

Two spaces and not a bar: the band behind the block is already the thing that
says \"this is a listing\", and a rule *and* a band is one mark too many. What
the indent buys is the gutter a printed listing has — the code does not start
at the same column the prose does, so the eye finds the block's left edge
without reading it.

Every row gets it, wrapped continuations included, which is why this is
`line-prefix' and not an indent written into the display string.")

(defparameter *org-frozen-quote-prefix* "▎ "
  "The bar down the left of a quote block. Drawn in the *overlay's own face*,
which is why the quote arm of `%org-frozen-block' sets one: a quote is set in
the comment face and its bar takes the same colour, so the whole block recedes
together rather than being grey text beside a bright rule.")

(defparameter *org-frozen-title-scale* 2.0
  "Type size of a `#+TITLE:' line, as a multiple of the body's.

Twice the body, against 1.5 for a level-1 heading, because a title is not a
heading: it names the document rather than a part of it, and the gap between it
and the first heading is what says so. NIL leaves it at body size.

`scale' is a line property wherever it lands, so this sizes the whole line —
see the note on `Overlay::scale'.")

(defparameter *org-frozen-subtitle-scale* 1.15
  "Type size of a `#+SUBTITLE:' line. Barely above the body on purpose: a
subtitle that competes with the title is not a subtitle.")

(defparameter *org-frozen-rule-width* 72
  "Characters wide a horizontal rule (`-----' alone on a line) is drawn.

A count and not \"the width of the pane\", because the image cannot know the
pane's width and the one number it *could* use — `text-width', which org claims
at 80 — is the measure the text is centred in rather than the measure it fills.
72 is a rule that stops short of the text column on both sides, which is what a
rule in a book does.")

(defparameter *org-frozen-table-separator* "  "
  "What goes between two columns of a table.

Two spaces, which is to say: **no vertical rules**. That is the printed
convention — a book's table has horizontal rules and white space, and the
vertical lines in an org table are there because a text editor has no other way
to say where a column ends. Once the columns are actually aligned, they are not
needed and they are noise.

Any glyph will do, `\" │ \"' included: the cell text a row is built from is
decoded before it is joined to this, so there is no encoding to be careful of.
See `%org-frozen-text', which is what buys that.")

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
  "The overlay properties a `#+begin_NAME' body wants, as a plist.

The ninth, and the only one that is a function rather than a value — because it
is a `case' and not an alist. The same shape `%org-modern-render' has, and for
the same reason: giving a kind
of block a new look is a line in its arm and no change to anything that applies
it. Two keys are read by the caller rather than by the renderer —

  :HIGHLIGHT  set the body in its own language;
  :HIDE       the body is not part of the page at all —

and they ride in the same plist because the alternative is a second table that
has to be kept in step with this one. The renderer has no arm for either, so
both stop in the image; that is the whole point of an overlay's property list
living in Lisp.

Two shapes, and the difference between them is the whole of it. A *listing* gets
a band and an indent: `line-background' paints the line edge to edge, which is
what `background' cannot do — a background paints the cells a range covers, so a
block drawn with one is a stripe as ragged as its own code. A *quote* gets a
rule down its left and is set in the comment face, italic, so the block recedes
together — including its bar, which `line-prefix' draws in the overlay's own
face.

The fallback is a band. A block of an unknown kind is *some* kind of set-apart
text, and drawing it as ordinary prose would lose the one thing its author was
certainly saying."
  (cond
    ((string-equal name "src")
     (list :highlight t
           'line-background *org-frozen-code-face*
           'line-prefix *org-frozen-code-prefix*))
    ((member name '("example" "verse") :test #'string-equal)
     (list 'line-background *org-frozen-code-face*
           'line-prefix *org-frozen-code-prefix*))
    ((string-equal name "quote")
     (list 'face "comment"
           'slant 'italic
           'line-prefix *org-frozen-quote-prefix*))
    ;; `#+begin_comment' is a note to the author and `#+begin_export' is
    ;; someone else's markup. Neither is on the page.
    ((member name '("comment" "export") :test #'string-equal)
     (list :hide t))
    (t (list 'line-background *org-frozen-code-face*))))

;;; ---------------------------------------------------------------------------
;;; Buffer text on its way back out
;;;
;;; **Every string this file takes out of the buffer and hands to a primitive
;;; goes through `%org-frozen-text' first.** That is one function and one bug,
;;; and the bug is worth the words because it is invisible: the page is drawn
;;; correctly right up until somebody puts an em dash in the title — which the
;;; curriculum in `examples/math/' does, in its first line.
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
;;; So the fix is to *decode*, in the image, before the string leaves it. Then
;;; what crosses the boundary is a real character string and the encoder does the
;;; right thing with it, which is the same thing it does with `◉'.
;;;
;;; Two consequences worth stating. Joining decoded text to a literal is now
;;; safe, so a table row can be padded and a separator can be any glyph at all —
;;; the constraint that would otherwise have forced this file's chrome to be
;;; ASCII simply is not there. And a decoded string's LENGTH is its width in
;;; *characters*, so the column arithmetic below needs no `%char-index' at all.
;;;
;;; ponytail: this is the local repair and not the general one. `TODO.org' calls
;;; the general one deliberately unfixed, and it is right to — the coherent
;;; version moves the whole model to character strings and deletes the two index
;;; conversions, and nothing can tell a byte-shaped string from a genuine
;;; Latin-1 one by looking at it. What this buys is that a *frozen page* is
;;; correct. org-modern is on the same path with the same bug and is not fixed
;;; here: a `*bold*' run or a `[[a][café]]' description is drawn straight from
;;; buffer text, and what you get is a wrong glyph that becomes right the moment
;;; the cursor reveals it. The upgrade path is this function, lifted into the
;;; standard library and called from both.

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

(defun %org-frozen-repeat (char n)
  "N copies of CHAR, as a string that can hold one outside Latin-1.

`:element-type 'character' is the whole point: a base string cannot hold `─',
and `make-string' asked for the default type may well be given one. A rule is a
whole line of its own and is joined to nothing, so it needs no decoding — it is
a literal, and a literal is the case the encoder was always right about."
  (make-string (max n 0) :initial-element char :element-type 'character))

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
  "Byte index in LINE where a headline's trailing `:tag:tag:' run begins,
counting the whitespace in front of it, or NIL.

The whitespace goes with the tags deliberately: replacing `  :paused:' with a
single space leaves one invisible cell at the end of the line, and replacing
only `:paused:' would leave the two spaces that were pushing it to the right.

A byte index because `line-string' and `buffer-string' hand out UTF-8 bytes and
every caller here converts with `%char-index' — the same convention
`%org-modern-line' reports in."
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
;;; about it — and the one place they would disagree is exactly the place it
;;; matters, because a line one pass hides and another draws over is a blank row
;;; with a glyph on it.
;;;
;;; Line-oriented for org-modern's reason: org decides what a line *is* from its
;;; first few characters, and only blocks and drawers carry state across lines.

(defun %org-frozen-lines ()
  "The buffer as a vector of (STRING BEGIN END), which is `%org-lines' with
random access. A vector and not the list, because the hiding pass asks for
\"the line before this run\" and doing that with NTH over a textbook is
quadratic for no reason at all."
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
drawing pass tests rather than a separate flag.

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
;;; The overlays
;;;
;;; `:org-frozen' holds the *kind*, which is both the mark saying "this one is
;;; mine" and what the fold pass needs to find its own again. One property, two
;;; jobs — the shape `:org-modern' and `:org-image' already have, and the reason
;;; none of the three can eat another's overlays.

(defun %org-frozen-overlay (kind beg end &rest props)
  "One frozen overlay of KIND over BEG..END carrying PROPS, or NIL.

NIL for an empty range, which `make-overlay' already answers and which happens
more here than anywhere else: a block with no body, a keyword run at the very
top of the file, a table row that is nothing but bars.

PROPS is applied with `%org-modern-apply', deliberately — it is the one place a
property name is not written out, and reusing it means a property this file
invents costs a line in the caller and nothing in the machinery. Keys the
renderer has no arm for (`:highlight' below) stay in the image and are ignored,
which is the whole point of the plist living in Lisp."
  (let ((ov (and (< beg end) (make-overlay beg end))))
    (when ov
      (overlay-put ov :org-frozen kind)
      (%org-modern-apply ov props)
      ov)))

(defun %org-frozen-overlays (&optional (kind nil))
  "Handles of the overlays this file made, of KIND or of every kind."
  (let ((out nil))
    (dolist (o (overlays-in (point-min) (point-max)) (nreverse out))
      (let ((k (overlay-get (first o) :org-frozen)))
        (when (and k (or (null kind) (eq k kind)))
          (push (first o) out))))))

;;; ---------------------------------------------------------------------------
;;; Hiding the machinery
;;;
;;; A `fold' overlay is the only payload that makes rows *stop existing* —
;;; everything else replaces cells with cells — so it is the only honest way to
;;; take a property drawer off a printed page. A `display' of nothing would
;;; leave four blank rows where the drawer was, which is not what a book does
;;; with a drawer; it is what a book does with four blank lines.
;;;
;;; The one rule to know, from `fold_hiding': a fold hides the lines *after* its
;;; own first one. So hiding lines A..B means a fold anchored at the end of line
;;; A-1, and a run that starts at line 0 has nowhere to anchor — the first line
;;; of a buffer can never be folded by anything. That case gets a `display' of a
;;; single space instead and costs one blank row at the top of the file, which
;;; is why `#+TITLE:' is *typeset* rather than hidden: it is the keyword most
;;; likely to be line 0, and typesetting it makes the question not arise.
;;;
;;; Consecutive machinery lines are merged into one fold. That is not thrift —
;;; a heading's drawer is four lines and four folds would work — it is that the
;;; renderer resolves overlays per line, so fewer and longer is strictly less
;;; work per frame for the rest of the document's life.

(defun %org-frozen-hide-run (v a b)
  "Make lines A..B of V stop occupying rows. Answers how many overlays it took.

The line-0 case is the whole reason this is a function: a fold hides the lines
*after* its own, so the first line of a buffer cannot be folded by anything at
all. It gets a `display' of one space — a blank row, which is the honest
second-best — and the fold picks up from line 1."
  (let ((made 0))
    (when (zerop a)
      (destructuring-bind (line begin end) (aref v 0)
        (declare (ignore line))
        (when (%org-frozen-overlay :hidden begin end 'display " ")
          (incf made)))
      (setf a 1))
    (when (<= a b)
      (when (%org-frozen-overlay :hidden
                                 (third (aref v (1- a)))
                                 (third (aref v b))
                                 'fold t)
        (incf made)))
    made))

(defun %org-frozen-hide (v roles blocks)
  "Take every line the page does not have off the page. Answers how many
overlays it took.

**Every fold this file makes is made here**, and that is what `org-frozen-cycle'
depends on: a fold is an overlay and org's own heading cycle deletes any it
finds, so there has to be exactly one function that can put all of them back.
The block bodies are folded from here for that reason and no other — a
`#+begin_comment' is machinery like a drawer is, whatever pass happened to
notice it."
  (let ((n (length roles)) (made 0) (i 0))
    (loop while (< i n)
          do (cond ((not (eq (aref roles i) :hidden)) (incf i))
                   (t (let ((a i))
                        (loop while (and (< i n) (eq (aref roles i) :hidden))
                              do (incf i))
                        (incf made (%org-frozen-hide-run v a (1- i)))))))
    ;; ...and the bodies of the blocks that are not part of the document. Their
    ;; delimiters were :HIDDEN and are already gone; this is the text between.
    (dolist (b blocks made)
      (destructuring-bind (name lang first last) b
        (declare (ignore lang))
        (when (and (<= first last) (getf (%org-frozen-block name) :hide))
          (incf made (%org-frozen-hide-run v first last)))))))

;;; ---------------------------------------------------------------------------
;;; The title, the tags, and the rules
;;;
;;; Three one-line typesetting decisions, together because each is two lines of
;;; code and none of them wants a section.

(defun %org-frozen-headers (v roles)
  "Typeset `#+TITLE:' and `#+SUBTITLE:', take the tags off headings, and draw
`-----' as a rule."
  (dotimes (i (length roles))
    (destructuring-bind (line begin end) (aref v i)
      (case (aref roles i)
        ;; A *title*, not a heading: twice the body against a level-1's 1.5, and
        ;; the `#+TITLE:' itself replaced by nothing rather than folded. `scale'
        ;; is a line property, so putting it on the overlay that covers the
        ;; keyword sizes the whole line — the same trick `%org-modern-render'
        ;; plays with a heading's stars.
        (:title
         (let ((value (%org-frozen-keyword line "TITLE")))
           (when (plusp (length value))
             (%org-frozen-overlay :title begin end
                                  ;; Through the funnel even though nothing is
                                  ;; joined to it: the value is buffer text, and
                                  ;; buffer text reaches the renderer intact only
                                  ;; as a base string. A title is the first line
                                  ;; of the document and the likeliest place in
                                  ;; it to hold an em dash.
                                  'display (%org-frozen-text value)
                                  'scale *org-frozen-title-scale*
                                  'weight 'bold))))
        (:subtitle
         (let ((value (%org-frozen-keyword line "SUBTITLE")))
           (when (plusp (length value))
             (%org-frozen-overlay :title begin end
                                  'display (%org-frozen-text value)
                                  'scale *org-frozen-subtitle-scale*
                                  'slant 'italic
                                  'face "comment"))))
        ;; A tag is a filing decision, not something the page says. One space
        ;; rather than nothing, because a `display' of the empty string *clears*
        ;; the property — see `command_for' in `zemacs-lisp', where that is
        ;; argued for deliberately — and a config that means "hide this" is
        ;; expected to say so with a space. At the end of a line one space and
        ;; no space look identical.
        (:heading
         (let ((at (%org-frozen-tags line)))
           (when at
             (%org-frozen-overlay :tags
                                  (+ begin (%char-index line at))
                                  end
                                  'display " "))))
        ;; A rule is chrome and takes the markup face, which is the face org's
        ;; own delimiters are drawn in — so a theme that dims its punctuation
        ;; dims this too, and nothing new has to be coloured.
        (:rule
         (%org-frozen-overlay :rule begin end
                              'display (%org-frozen-repeat #\─ *org-frozen-rule-width*)
                              'face "markup"))
        (t nil)))))

;;; ---------------------------------------------------------------------------
;;; Blocks
;;;
;;; The delimiters are already gone — `%org-frozen-scan' called them :HIDDEN and
;;; the fold pass took them — so what is left is to make the body *look* like
;;; what it is. Two shapes, and the difference between them is the whole of it:
;;;
;;;   a listing  band + indent, and set in its own language.
;;;   a quote    a rule down the left, set in the comment face, italic.
;;;
;;; `line-background' is what makes the first one possible at all. A `background'
;;; paints the cells a range covers, so a block drawn with one is a stripe as
;;; ragged as its own code; this paints the *line*, edge to edge, so a block
;;; reads as a block. That property exists for exactly this and this is its
;;; first customer.
;;;
;;; ...and then the interesting problem, which is the highlighting.
;;;
;;; A buffer has one language. Org has no tree-sitter grammar at all — it is
;;; hand-scanned in `crates/syntax/src/org.rs' — so the highlighting thread that
;;; colours every other buffer has nothing to say about a `#+begin_src python',
;;; and could not say it in Python even if it did. `tree-sitter-highlight' has an
;;; injection mechanism for precisely this and it is deliberately switched off
;;; (`HighlightConfiguration::new(.., "", "")'), which would in any case not help
;;; a language that has no grammar to inject from.
;;;
;;; So the block's body is highlighted the only way it can be: by asking for it.
;;; `(highlight LANG TEXT)' is `zemacs_syntax::highlight' with a Lisp face on it
;;; — a **pure function** taking two strings and answering `((BEG END FACE) ...)'
;;; in char offsets, touching no editor and taking no lock. It is the one thing
;;; here that had to be built in Rust, it is fourteen lines, and it is not about
;;; org: it is "colour this string as that language", which is what a markdown
;;; mode, a docstring, or a diff view would each want next.
;;;
;;; Each run becomes an ordinary `face' overlay, and an overlay's face beats the
;;; syntax highlight underneath — so the org scanner's opinion about the block is
;;; simply overruled, per run, with no coordination between them.
;;;
;;; ponytail: one overlay per token run, and a long listing has a few hundred.
;;; `Overlays' is a `Vec' scanned linearly per drawn line, which is the ceiling
;;; that module already names for itself; the upgrade path is the same interval
;;; tree, and it is shared with avy rather than owed to this file.

(defun %org-frozen-uncomma (v first last)
  "Take the escaping comma off the `,#+' and `,*' lines of a block body.

Org's rule for a block that quotes org: a line that would otherwise close the
block, or start a heading inside it, is written with a leading comma. A printed
page shows what was meant, so the comma goes.

Two characters covered and one drawn, rather than one covered and none: a
`display' of the empty string clears the property instead of hiding anything,
so the substitution has to keep the character after the comma to have something
to be. `docs/curriculum.org' is a document made almost entirely of this."
  (loop for i from first to last
        do (destructuring-bind (line begin end) (aref v i)
             (declare (ignore end))
             (let ((j (or (position-if-not #'%org-blank-p line) (length line))))
               (when (and (< (1+ j) (length line))
                          (char= (char line j) #\,)
                          (member (char line (1+ j)) '(#\# #\*)))
                 (%org-frozen-overlay :uncomma
                                      (+ begin (%char-index line j))
                                      (+ begin (%char-index line (+ j 2)))
                                      'display (%org-frozen-text
                                                (subseq line (1+ j) (+ j 2)))))))))

(defun %org-frozen-highlight (lang beg end)
  "Set BEG..END in LANG, one `face' overlay per run. Answers how many.

The text is fetched with one `buffer-substring' and handed over whole, because
`highlight' parses whatever it is given: asking per line would reparse the block
once per line and would also get it *wrong*, since a Python triple-quoted string
or a Rust block comment is not a line-local fact.

Offsets come back relative to the text, in characters, which is the unit
`make-overlay' takes — so the arithmetic is one addition and there is no byte
conversion anywhere in this function. That is not luck: it is why the primitive
answers in char offsets rather than in the byte offsets tree-sitter works in.

...and it is exactly why the text is *decoded* on the way out. `buffer-substring'
answers one character per UTF-8 byte, and handing that over unchanged would have
the parser see a longer string than the one in the buffer — every span after the
first accented character in a comment or a docstring would land a byte or two to
the right of the run it describes, and the block would look subtly misaligned
rather than broken. `%org-frozen-text' is what makes the two sides count the same
characters."
  (let ((made 0))
    (when lang
      (dolist (span (highlight lang (%org-frozen-text (buffer-substring beg end))))
        (destructuring-bind (a b face) span
          (when (%org-frozen-overlay :code (+ beg a) (+ beg b) 'face face)
            (incf made)))))
    made))

(defun %org-frozen-blocks (v blocks)
  "Draw every block body. Answers how many runs were highlighted."
  (let ((lit 0))
    (dolist (b blocks lit)
      (destructuring-bind (name lang first last) b
        (let ((look (%org-frozen-block name)))
          ;; An empty body — `#+begin_src' immediately followed by `#+end_src' —
          ;; has no lines to band and nothing to highlight; both delimiters are
          ;; folded, so the block leaves no trace at all, which is right for a
          ;; block with nothing in it. A hidden one is `%org-frozen-hide''s, for
          ;; the reason written there: every fold is made in one place.
          (when (and (<= first last) (not (getf look :hide)))
            (let ((beg (second (aref v first)))
                  (end (third (aref v last))))
              (apply #'%org-frozen-overlay :block beg end look)
              (%org-frozen-uncomma v first last)
              (when (getf look :highlight)
                (incf lit (%org-frozen-highlight (%org-frozen-language lang)
                                                 beg end))))))))))

;;; ---------------------------------------------------------------------------
;;; Tables
;;;
;;; An org table is aligned by *org*, in the buffer, by rewriting the text — and
;;; this mode never rewrites anything, so alignment here has to be a drawing.
;;; It is: one `display' overlay per row, holding the row's own cell text with
;;; the padding recomputed. A substitution replaces the cells it covers, so
;;; wrapping, the cursor and the selection all follow it, and the columns line
;;; up whatever the author typed.
;;;
;;; What it draws is a *book's* table and not a text editor's:
;;;
;;;   - no vertical rules. The bars in an org table are there because plain text
;;;     has no other way to say where a column ends; once the columns are
;;;     aligned, they are noise. See `*org-frozen-table-separator*'.
;;;   - `|---+---|' becomes one unbroken rule, as wide as the table.
;;;   - the rows above the first rule are the header, and are set bold.
;;;
;;; One overlay per row rather than one per cell: a cell is repadded *and*
;;; re-joined, so what is being replaced is the row, and three overlays per cell
;;; would be the same drawing said less directly and re-resolved per frame.
;;;
;;; Everything here works on **decoded** cell text — `%org-frozen-text' up front,
;;; once per cell — which is what makes a column's width its LENGTH and lets the
;;; separator be any glyph at all. Doing it the other way round, padding
;;; byte-shaped text and converting widths with `%char-index', is the version
;;; that draws `café' as five columns and aligns the table one short.
;;;
;;; ponytail: a `|' inside a cell splits it, and a column's width is its
;;; character count — so a table of CJK, whose glyphs are two cells wide, aligns
;;; by one column too few per wide character. Org escapes an embedded bar as
;;; `\\vert' and the renderer already knows what `char_cells' means; the upgrade
;;; path for the second is that function, answered from Lisp.

(defun %org-frozen-cells (line)
  "The `|'-delimited cells of a table row, as decoded strings with their padding
already trimmed off.

Trimmed and decoded here rather than by the caller because *every* caller wants
both: a column's width is the width of its content in characters, and the
content is what gets redrawn. Whatever whitespace the author used to align it by
hand is exactly the thing this mode is replacing."
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

(defun %org-frozen-row (cells widths)
  "One table row drawn: its CELLS, padded to WIDTHS, joined by the separator.

The last column is deliberately not padded. Trailing spaces are cells like any
other — the selection would swallow them and a `$' would land past the end of
what you can see — and nothing is ever to the right of the last column for them
to align."
  (let ((parts nil) (n (length widths)))
    (dotimes (j n)
      (let ((text (or (nth j cells) "")))
        (push text parts)
        (when (< j (1- n))
          (push (make-string (max (- (nth j widths) (length text)) 0)
                             :initial-element #\Space)
                parts)
          (push *org-frozen-table-separator* parts))))
    (apply #'concatenate 'string (nreverse parts))))

(defun %org-frozen-table (v first last)
  "Draw the table occupying lines FIRST..LAST of V."
  (let* ((rows (loop for i from first to last collect (aref v i)))
         (cellsets (mapcar (lambda (r) (%org-frozen-cells (first r))) rows))
         (rules (mapcar (lambda (r) (%org-frozen-rule-row-p (first r))) rows))
         (ncols (reduce #'max (mapcar #'length cellsets) :initial-value 0))
         (widths (make-list ncols :initial-element 0))
         ;; The header is everything above the *first* rule, and only when there
         ;; is one: a table with no rule has no header, it has rows. NIL rather
         ;; than 0 so the two cases read differently below.
         (header (position t rules)))
    (when (plusp ncols)
      ;; Widths come from the rows that carry content. A rule row's cells are
      ;; runs of dashes as long as the author felt like making them, and letting
      ;; one set a column's width would make the table as wide as its own
      ;; punctuation.
      (loop for cs in cellsets for rule in rules
            unless rule
              do (loop for c in cs for j from 0
                       do (setf (nth j widths) (max (nth j widths) (length c)))))
      (let ((total (+ (reduce #'+ widths :initial-value 0)
                      (* (max 0 (1- ncols)) (length *org-frozen-table-separator*)))))
        (loop for r in rows
              for cs in cellsets
              for rule in rules
              for i from 0
              do (destructuring-bind (line begin end) r
                   (declare (ignore line))
                   (if rule
                       (%org-frozen-overlay :table begin end
                                            'display (%org-frozen-repeat #\─ total)
                                            'face "markup")
                       (%org-frozen-overlay
                        :table begin end
                        'display (%org-frozen-row cs widths)
                        ;; A header is *weight*, not colour: the rule under it
                        ;; already says where it ends, and a coloured header row
                        ;; in a printed table reads as a highlight rather than
                        ;; as a heading.
                        'weight (if (and header (< i header)) 'bold nil)))))))))

(defun %org-frozen-tables (v roles)
  "Draw every table in the buffer, each as one run of consecutive :TABLE lines."
  (let ((n (length roles)) (i 0))
    (loop while (< i n)
          do (cond ((not (eq (aref roles i) :table)) (incf i))
                   (t (let ((a i))
                        (loop while (and (< i n) (eq (aref roles i) :table)) do (incf i))
                        (%org-frozen-table v a (1- i))))))))

;;; ---------------------------------------------------------------------------
;;; LaTeX, without the one rule that only makes sense while typing
;;;
;;; `org-latex-preview-new' — which org-mode's own body already ran, one level
;;; up — deliberately skips the fragment point is inside, because you are still
;;; typing it and `$\\alph' is a LaTeX error rather than an equation.
;;;
;;; Nothing is typed here, so that rule has nothing to protect and one real
;;; cost: enter the mode with the cursor inside an equation and that one
;;; equation stays as source, on a page where everything else is typeset. This
;;; is the same pass with the same two queries and that single test removed.

(defun %org-frozen-latex ()
  "Typeset every fragment that has no preview yet, the one under point included."
  (when (and (boundp '*org-latex-auto*)
             *org-latex-auto*
             (fboundp '%org-latex-draw))
    (let ((have (%org-latex-previewed-ranges)))
      (dolist (f (reverse (latex-fragments)))
        (destructuring-bind (fbeg fend display) f
          (declare (ignore display))
          (when (notany (lambda (r) (and (< (car r) fend) (> (cdr r) fbeg))) have)
            ;; A machine with no `latex' says so once, from inside the
            ;; primitive, and this stops asking — exactly as the pass it is
            ;; modelled on does.
            (unless (%org-latex-draw fbeg fend)
              (setf *org-latex-auto* nil)
              (return))))))))

;;; ---------------------------------------------------------------------------
;;; The commands

(defun org-frozen-clear ()
  "Take the whole rendering off, showing org's machinery again.

Exactly its own overlays: org-modern's bullets, the LaTeX previews and the
figures are all still wanted, and `remove-overlays' would take every one of
them. That is what `:org-frozen' is for and it is the third file in the runtime
to need the trick."
  (let ((ovs (%org-frozen-overlays)))
    (mapc #'delete-overlay ovs)
    (length ovs)))

(defun org-frozen-refresh ()
  "Redraw the page.

Its own overlays first, so this doubles as `refresh' and can be pressed as often
as you like. One scan, one pass per kind of thing, and a `buffer-substring' per
source block — which is why it is a *command* and not something on a hook: it is
affordable exactly because the document it renders cannot change."
  (org-frozen-clear)
  (let* ((v (%org-frozen-lines))
         (scan (%org-frozen-scan v))
         (roles (car scan))
         (blocks (cdr scan)))
    ;; Blocks before the fold pass would work too; this order is chosen so that
    ;; a `#+begin_comment' — which the block pass folds — is folded by the same
    ;; kind of overlay as every other piece of machinery and comes off with it.
    (%org-frozen-hide v roles blocks)
    (%org-frozen-headers v roles)
    (%org-frozen-tables v roles)
    (let ((lit (%org-frozen-blocks v blocks)))
      ;; Ends on the message so the form answers NIL: `eval-string' echoes the
      ;; value of the last form and would otherwise wipe out what this said.
      (message (format nil "~d block~:p, ~d highlighted run~:p"
                       (length blocks) lit)))))

(defun org-frozen-cycle ()
  "TAB in a frozen buffer: org's three-state heading cycle, with the machinery
put back afterwards.

Two things make this more than a call to `org-cycle'. A fold is an overlay, so
`unfold-region' takes out *every* fold overlapping the subtree — including the
ones holding this document's drawers shut. And `%org-cycle-state' tells its
three states apart by looking at the folds it finds, so a drawer's fold inside
an open subtree makes it report CHILDREN when the subtree is plainly open, and
the cycle sticks.

Both are answered by the same two lines: take the machinery folds off, let
`org-cycle' see a buffer folded only by itself, and put them back. Cheap enough
for a key you press — the pass is one buffer scan, which is what pressing `SPC
m m' costs — and it leaves the *display* overlays alone, so the tables and the
listings under a heading you fold and unfold are never redrawn at all."
  (mapc #'delete-overlay (%org-frozen-overlays :hidden))
  (org-cycle)
  (let* ((v (%org-frozen-lines))
         (scan (%org-frozen-scan v)))
    (%org-frozen-hide v (car scan) (cdr scan))))

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
  "Leaving the mode puts the document back: the rendering comes off and the
buffer becomes editable again.

Guarded on the buffer, for the reason `*org-frozen-buffer*' gives. Taking the
read-only flag off the wrong buffer would be the dangerous half — a dired
listing quietly becoming writable is exactly the kind of bug that shows up as
data loss much later."
  (let ((me (or (buffer-file-name) (buffer-name))))
    (when (equal me *org-frozen-buffer*)
      (org-frozen-clear)
      (set-buffer-read-only nil)
      (setf *org-frozen-buffer* nil))))

(defun org-frozen-toggle ()
  "Switch between reading this org file and editing it."
  (if (derived-mode-p 'org-frozen-mode) (org-mode) (org-frozen-mode)))

;;; ---------------------------------------------------------------------------
;;; The mode
;;;
;;; Derived from `org-mode', which is the whole reuse story in one word: entering
;;; it runs org-mode's body first — org-modern on, LaTeX previewed, figures drawn,
;;; `*org-mode-functions*' run, so `math.lisp' still recognises a curriculum —
;;; and everything in the body below is what this mode adds *on top*. Nothing is
;;; copied and there is nothing to drift.
;;;
;;; What it inherits and what it does not is worth being precise about, because
;;; the two mechanisms in `modes.lisp' differ: `set-mode-local' claims and
;;; `define-mode-key' bindings are inherited down the chain (so org's 80-column
;;; measure and org-fold's TAB arrive for free), while a plain `define-key' on
;;; `"org-mode"' is not (so `RET' on a link has to be said again, below).

(define-derived-mode org-frozen-mode org-mode
  ;; Real, and enforced in core rather than by taking keys away: `i', `x', `p',
  ;; `u', a paste and an `insert' from Lisp are all refused by the one guard in
  ;; `Editor::apply', and `i' additionally says so instead of parking you in a
  ;; mode where every keystroke bounces. Everything that is *not* an edit still
  ;; works — every motion, `/', `n', the folds, `M-x' — which is the difference
  ;; between a read-only buffer and a disabled one.
  (set-buffer-read-only t)
  (setf *org-frozen-buffer* (or (buffer-file-name) (buffer-name)))
  ;; org-modern is already on — org-mode's body saw to it — but a fragment may
  ;; be *revealed* under the cursor from before the mode changed, and a revealed
  ;; fragment is exactly the thing this mode must never show. Re-hiding the one
  ;; is a couple of `overlay-put's; a full `org-modern-refresh' would be a second
  ;; scan of the buffer to fix at most one overlay.
  (when *org-modern-revealed*
    (%org-modern-rehide *org-modern-revealed*)
    (setf *org-modern-revealed* nil))
  (%org-frozen-latex)
  (org-frozen-refresh))

;;; Reveal-on-cursor is the one org-modern behaviour a *renderer* must not have:
;;; the whole promise of this mode is that the machinery is not on the page, and
;;; markup that reappears because you moved the cursor onto it breaks that
;;; promise once per keystroke.
;;;
;;; Said as a list in org-modern rather than as a test for this mode, and pushed
;;; onto from here — the shape `*org-mode-functions*', `*after-change-functions*'
;;; and `*fold-subtree-functions*' all have. org-modern does not know this file
;;; exists, and a second mode wanting the same thing is one more PUSHNEW.
(defvar *org-modern-appear-inhibit-modes* nil)
(pushnew "org-frozen-mode" *org-modern-appear-inhibit-modes* :test #'string=)

;;; The gutter is claimed in `init.lisp' beside org's, with the rest of the
;;; `set-no-gutter-modes' list — one list, in the file that owns it, rather than
;;; a second copy here that would drift the first time a mode is added to
;;; either. Same argument as the reveal above: line numbers are for a document
;;; you are going to talk about by line, and nothing here is.

;;; ---------------------------------------------------------------------------
;;; Redrawing after a write that was not yours
;;;
;;; A frozen buffer is read-only *to the keyboard* and is still written to — see
;;; `with-inhibited-read-only' in the shim, and the two callers this mode was
;;; built for: a transcribed handwritten solution landing in a problem's Response
;;; section, and a problem's `:ZEMACS_STATUS:' being toggled in place. Both are
;;; `replace-region', and both leave the page describing a document that has
;;; moved under it — a fold anchored one line short, a table whose widest cell
;;; just changed.
;;;
;;; So the *change* hook is where a renderer can afford what an editor cannot.
;;; `org-modern.lisp' deliberately does not rescan here and says why at length: a
;;; buffer pass per keystroke would put a `%do' per bullet between you and your
;;; next character. Nothing types into this one. Every change it will ever see is
;;; a program writing a paragraph, which is exactly the moment a full redraw is
;;; both cheap and necessary.
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
;;; TAB and the leader, and nothing else — a mode you cannot type in needs very
;;; few keys, and every one it does not claim is one that still does what it does
;;; everywhere else.

;;; org-fold binds `<tab>' for `org-mode' with `define-mode-key', so this mode
;;; inherits `org-cycle' and would work. It is rebound anyway, and the reason is
;;; in `org-frozen-cycle''s docstring: a fold is an overlay, and org's cycle and
;;; this file's machinery are both made of them.
(define-key "org-frozen-mode" "<tab>" "org-frozen-cycle")

;;; `RET' follows a link here as it does in org-mode. Said again rather than
;;; inherited because `org-modern.lisp' binds it with a plain `define-key', which
;;; is keyed by the exact mode name — and following a link is *more* useful in a
;;; document you are reading than in one you are writing.
(define-key "org-frozen-mode" "<ret>" "org-open-at-point")
(define-key "org-frozen-mode" "SPC m o" "org-open-at-point")

(define-key "org-frozen-mode" "SPC m m" "org-frozen-refresh")
(define-key "org-frozen-mode" "SPC m z" "org-frozen-toggle")
;;; ...and the way *in*, from an ordinary org buffer. One key, both directions.
(define-key "org-mode" "SPC m z" "org-frozen-toggle")
