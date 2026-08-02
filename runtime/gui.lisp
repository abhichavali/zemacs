;;;; Scenes — a page in pixels, described here and laid out by Rust.
;;;;
;;;; Everything else the editor draws is cells. A scene is the other thing a
;;;; window can show: a *tree*, measured in pixels, wrapped by the font rather
;;;; than counted in columns, and read-only for real. `docs/gui.org' is the
;;;; argument for it and `crates/gui' is the layout engine; this file is the
;;;; whole Lisp face of it, and it is four builders, a click table and one verb.
;;;;
;;;; The mechanism is one form. A scene is *described in one form and swapped in
;;;; whole* — never mutated node by node — so `scene-set' takes a printed node
;;;; and installs it on the live buffer, and taking a page down is `(scene-set)'
;;;; with nothing to show. There is no scene handle, no node handle, and nothing
;;;; to keep in step with the document.
;;;;
;;;; ** Two languages with the same shape, and the difference is the point
;;;;
;;;; What you write here is Common Lisp:
;;;;
;;;;   (scene-set
;;;;     (block :pad 48 :gap 12 :width (pct 100)
;;;;       (text (run "The Rank-Nullity Theorem" :size 200 :bold t))
;;;;       (text (run "For a linear map ") (run "T : V → W" :italic t)
;;;;             (run ", the dimensions satisfy:"))
;;;;       (image (latex-preview "\\dim V = \\operatorname{rank} T + \\dim\\ker T"))))
;;;;
;;;; `block', `text', `run' and the rest are ordinary functions: they evaluate
;;;; their arguments and answer a *string*, the printed wire form. So
;;;; `(image (latex-preview "..."))' works for the reason any call works — the
;;;; preview is rendered first and answers an id — and what crosses into Rust is
;;;; `(image 7314)'. The reader on the other side, `crates/lisp/src/scene.rs',
;;;; cannot evaluate anything; every id, length and colour reaches it already
;;;; reduced to an integer.
;;;;
;;;; That is also why these are functions and not a macro. A document is built by
;;;; a loop over a scanner's output, so the children of a block are a *list* the
;;;; program computed — `(apply #'block :pad 48 (mapcar #'%paragraph lines))' —
;;;; and a macro would have made the one case that matters the awkward one.
;;;;
;;;; ** Strings are characters here, not bytes
;;;;
;;;; The one sharp edge, and it will cut the first document with an em dash in
;;;; it. Lisp holds buffer text as UTF-8 *bytes*, one character per byte — that
;;;; is what `f_query' in `shim.c' writes up and what makes `search-forward'
;;;; work — while the encoder that carries this file's output back to Rust
;;;; encodes each character as UTF-8. Hand it bytes and they are encoded twice:
;;;; `—' arrives as `â€"' and the page is wrong in a way no assertion about the
;;;; tree would notice. So decode buffer text with `utf8-text' before it goes
;;;; into a `run'. A literal in a file you `load'ed is already characters and
;;;; needs nothing.
;;;;
;;;; ** A click is an integer
;;;;
;;;; A `:tag' is an integer this file chose, and Rust does nothing with it except
;;;; hand it back when a click lands inside — the same contract an `OverlayId'
;;;; has, and for the same reason: an arbitrary Lisp value never has to survive a
;;;; round trip through C as printed source. Write the closure in the form:
;;;;
;;;;   (run "done" :face "link" :tag (lambda () (math-toggle-status problem)))
;;;;
;;;; and the answer to "what does that mean" stays where the widget is written.
;;;; `%scene-click' below is what the editor calls with the integer.

(in-package :zemacs)

;;; `block' is a Common Lisp special operator and this package uses CL, so
;;; `(defun block ...)' is a locked-package error rather than a definition. The
;;; name is taken away from CL here because `docs/gui.org' spells the node
;;; `block' and a scene full of `scene-block' would be a document written around
;;; an implementation detail.
;;;
;;; What it costs: a file loaded *after* this one cannot write `(block name
;;; ...)' as the special operator — `(cl:block name ...)' still can, and
;;; `return-from' out of a `defun', a `loop' or a `dolist' is untouched, since
;;; those blocks are established by macros that name CL's symbol themselves.
;;; Nothing in `runtime/' writes one; this is the only thing in the tree that
;;; takes a name back from CL, and it is written down here because a package
;;; that quietly means something different is the worst kind of surprise.
(eval-when (:compile-toplevel :load-toplevel :execute)
  (shadow "BLOCK" "ZEMACS"))

;;; ---------------------------------------------------------------------------
;;; Printing a value the way the reader on the other side wants it
;;;
;;; Every one of these answers a *string*, and the string is Lisp source. They
;;; are deliberately not `prin1': `prin1' upcases every symbol, respects
;;; `*print-base*' and truncates at `*print-length*', so a config that had set
;;; any of the three would print a page nobody could read back. The parser
;;; tolerates the upcasing — it has to, since a form the image printed is the
;;; usual case — but emitting what we mean costs nothing and depends on nothing.

(defun %scene-string (text)
  "TEXT as a Lisp string literal, escaped exactly the way `zemacs_rpc::lisp::string'
escapes one — a backslash before a backslash or a quote, and nothing else
special.

That is not a coincidence and must not become one: `scene.rs' undoes precisely
those two escapes, so the two halves of the bridge agree by construction. A NUL
becomes U+FFFD for the same reason that module replaces it — the string reaches
Rust as a C string, and a NUL in the middle would end the whole page there."
  (with-output-to-string (out)
    (write-char #\" out)
    (loop for c across (string text)
          do (cond ((char= c (code-char 0)) (write-char (code-char #xFFFD) out))
                   (t (when (or (char= c #\\) (char= c #\")) (write-char #\\ out))
                      (write-char c out))))
    (write-char #\" out)))

(defun %scene-int (n)
  "N as a decimal integer. `~d' rather than `~a' so a config that bound
`*print-base*' to 16 does not print `:pad 30' as `:pad 1e'. Rounded rather than
refused, because a size computed as `(* 1.5 32)' is a float and meant 48; the
bridge carries integers and the ranges are checked on the far side."
  (if (numberp n)
      (format nil "~d" (round n))
      (error "scene: expected a number, got ~s" n)))

(defun %scene-word (x)
  "X as one of the reader's bare symbols — `row', `center', `fill'. A keyword, a
symbol or a string all spell the same word, so `:dir :row', `:dir 'row' and
`:dir \"row\"' are one thing and a document may pick whichever reads best."
  (string-downcase (string x)))

(defun %scene-bool (x)
  "Strictly `t' or `nil', which is what the reader accepts: it refuses `:bold 0'
rather than reading it as bold, and generalised truth here would put that
mistake back."
  (if x "t" "nil"))

(defun %scene-length (x)
  "A length in the four spellings the grammar gives it: a number is pixels,
`(pct N)' is a percentage, and `fill' and `auto' are themselves."
  (cond ((numberp x) (%scene-int x))
        ;; What `pct' answered — already printed, and the only string a length
        ;; can be.
        ((stringp x) x)
        ((null x)
         (error "scene: a length is a number, (pct N), fill or auto — not NIL"))
        (t (%scene-word x))))

(defun %scene-face (face)
  "FACE as its number, which is its position in `face-list'.

A *name* rather than an RGB triple, and the same vocabulary every overlay
already uses: the theme owns the colours, so a page follows `load-theme' for
free and no second colour table exists. An integer passes through for the caller
that already has one, and NIL means the pane's own colour — which is what
`:face (when hot \"link\")' prints as when the condition failed."
  (cond ((null face) "nil")
        ((integerp face) (format nil "~d" face))
        (t (let ((i (position (string-downcase (string face)) (face-list)
                              :test #'string=)))
             (if i
                 (format nil "~d" i)
                 (error "scene: no such face: ~a" face))))))

(defun %scene-tag-id (tag)
  "A tag, as the integer the click table knows it by.

A function or a symbol is *parked* here rather than demanded of the caller, so
the closure can be written where the widget is: `:tag (lambda () ...)'. An
integer from `scene-tag' passes through, which is how two nodes share one tag."
  (cond ((null tag) "nil")
        ((integerp tag) (format nil "~d" tag))
        ((or (functionp tag) (symbolp tag)) (format nil "~d" (scene-tag tag)))
        (t (error "scene: a tag is an integer or a function, got ~s" tag))))

(defun %scene-image-id (id)
  "The id `latex-preview' or `image-file' answered.

NIL is refused loudly rather than drawn as a box of no size: both of those
primitives answer NIL when they could not render, with the reason already in the
status line, and a page that silently omitted the equation would hide the second
half of that report."
  (cond ((and (integerp id) (<= 0 id)) (format nil "~d" id))
        ((null id)
         (error "scene: no image id — `latex-preview' or `image-file' answered NIL"))
        (t (error "scene: an image id is an integer, got ~s" id))))

;;; ---------------------------------------------------------------------------
;;; The builders

(defun %scene-form (head spec lead args)
  "The printed form `(HEAD LEAD ARGS...)'.

SPEC is a plist of keyword -> the function that prints its value, which is what
makes every builder below one line: \"keywords in any order, all optional,
interleaved with children\" is the same loop four times, and it is the same loop
`args' in `scene.rs' runs to read them back. LEAD is an already-printed
positional — a run's string, an image's id — or NIL.

A NIL child is *dropped*, so `(block (when done (text ...)))' is how a document
says \"and this part only sometimes\" without building the argument list by
hand."
  (with-output-to-string (out)
    (write-char #\( out)
    (write-string head out)
    (when lead
      (write-char #\Space out)
      (write-string lead out))
    (loop while args
          do (let ((arg (pop args)))
               (cond
                 ((keywordp arg)
                  (let ((printer (getf spec arg)))
                    (unless printer
                      (error "scene: (~a ...) has no ~s" head arg))
                    (unless args
                      (error "scene: ~s in (~a ...) has no value after it" arg head))
                    (format out " :~(~a~) ~a" (symbol-name arg)
                            (funcall printer (pop args)))))
                 ;; A child that came out NIL. Deliberate, and documented above.
                 ((null arg))
                 ((stringp arg)
                  ;; Every child is what another builder answered, and every one
                  ;; of those starts with `('. Checking here turns `(text
                  ;; "hello")' — the mistake this API invites, since a builder
                  ;; answers a string and so does the reader of a buffer line —
                  ;; into a message naming the fix, rather than a parse error a
                  ;; whole page away saying it expected a run.
                  (unless (and (plusp (length arg)) (char= (char arg 0) #\())
                    (error "scene: (~a ...) takes what another builder answered, ~
                            not the string ~s — did you mean (run ~s)?"
                           head arg arg))
                  (write-char #\Space out)
                  (write-string arg out))
                 (t (error "scene: (~a ...) cannot take ~s" head arg)))))
    (write-char #\) out)))

(defun block (&rest args)
  "A box holding other nodes, and the only container there is.

  (block :pad 48 :gap 12 :dir :column :width (pct 100)
         (text (run \"hello\")))

`:pad' is inside every edge and `:gap' is between children — n children have
n-1 gaps, never one around them. `:dir' is `:column' (the default, down) or
`:row'; `:align' is `:start', `:center' or `:end' and moves the children
*across* `:dir', which is what centres a figure over its caption. `:width' and
`:height' are lengths; `:background' and `:border' are face names; `:tag' makes
the whole box clickable, padding included.

Children are drawn in the order they are written, and that is also stacking
order: a child's own background paints over its parent's border."
  (%scene-form "block"
               '(:pad %scene-int :gap %scene-int :dir %scene-word
                 :width %scene-length :height %scene-length :align %scene-word
                 :background %scene-face :border %scene-face :tag %scene-tag-id)
               nil args))

(defun text (&rest args)
  "A paragraph: one or more runs wrapped as a *single* flow.

  (text (run \"For a linear map \") (run \"T : V → W\" :italic t))

One node and not three, which is the whole reason a run exists: a bold word
mid-sentence must not start a new box, and the line breaks have to fall where
the font says they do rather than at the edges of the emphasis. `:align' moves
the *lines* within the paragraph's own width.

There are no hard breaks — every space is a break opportunity and nothing forces
one — so a code listing is one `text' per line, which is what a listing wants
anyway: each line carries its own syntax runs."
  (%scene-form "text" '(:align %scene-word) nil args))

(defun run (string &rest args)
  "A stretch of text set one way, inside a `text'.

  (run \"The Rank-Nullity Theorem\" :size 200 :bold t)

`:size' is a percentage of the body size — 200 is twice the prose around it —
because the bridge carries integers and because the renderer snaps a size to one
of a handful of steps anyway; that snapping is what bounds its glyph cache, so
175 and 200 may well be the same face. `:face' is a face name, `:tag' makes this
run and not the paragraph around it clickable.

STRING must be *characters*. If it came out of the buffer it is bytes — see
`utf8-text'."
  (%scene-form "run"
               '(:size %scene-int :bold %scene-bool :italic %scene-bool
                 :face %scene-face :tag %scene-tag-id)
               (%scene-string string) args))

(defun image-run (id &rest args)
  "A bitmap *in a sentence* — `$x_1$', a badge — inside a `text'.

  (image-run (latex-preview \"x_1\") :depth 6)

It wraps like a word, sits on the line's baseline and hangs `:depth' pixels
below it, which is what stops a subscript floating above the text it belongs to.
Sizes left unstated are filled in from the bitmap when the page is installed, so
`(image-run ID)' is usually the whole of it."
  (%scene-form "image-run"
               '(:width %scene-int :height %scene-int :depth %scene-int
                 :tag %scene-tag-id)
               (%scene-image-id id) args))

(defun image (id &rest args)
  "A bitmap at block level — a figure, or a display equation.

  (block :align :center (image (image-file \"fig-1.svg\")) (text (run \"Figure 1\")))

The same ids `latex-preview' and `image-file' answer for an overlay, so nothing
is rasterised twice. An unstated size is the bitmap's own."
  (%scene-form "image"
               '(:width %scene-int :height %scene-int :depth %scene-int)
               (%scene-image-id id) args))

(defun rect (&rest args)
  "A box with a size and maybe a colour, and no children — a rule, or a space.

  (rect :width (pct 60) :height 1 :fill \"markup\")

This is how a page says \"space here\" without an empty paragraph, which would
be a paragraph of no lines and therefore of no height at all. With no `:fill' it
paints nothing and is a spacer."
  (%scene-form "rect"
               '(:width %scene-length :height %scene-length :fill %scene-face)
               nil args))

(defun pct (n)
  "N percent, as a length: `:width (pct 100)'.

Of the parent's *content* box — inside its padding — so a child at `(pct 100)'
fills the room its parent left it and does not overhang."
  (format nil "(pct ~d)" (round n)))

;;; ---------------------------------------------------------------------------
;;; Text out of a buffer

(defun utf8-text (bytes)
  "BYTES — buffer text, one character per UTF-8 byte — as the characters it
actually spells.

`(buffer-string)', `(line-string 3)' and every other reader answer bytes, and
the encoder that carries a scene back to Rust encodes each *character* as UTF-8.
So a line with an em dash in it goes over twice-encoded and is drawn as the
Latin-1 reading of its own encoding — three wrong glyphs where the file has one.
Decoding here, before the string leaves the image, is what makes the page agree
with the file behind it.

ASCII is returned unchanged and untouched, which is both the fast path and the
common one. An invalid or truncated sequence is passed through one character at
a time rather than dropped: this is a *renderer*, and text it cannot make sense
of should still be visible.

ponytail: this is `%org-frozen-text' in `modes/org-frozen.lisp', a second time.
That file needed it before this one existed, for the same reason and with the
same body. Ceiling: two copies of a UTF-8 decoder, which agree today. The
upgrade path is that file calling this one — it loads later, so nothing but the
edit is in the way — and the coherent fix underneath both is the one
`docs/boundary.org' names on `f_query': decode in the shim and delete the byte
model entirely."
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

;;; ---------------------------------------------------------------------------
;;; What a click means
;;;
;;; The same shape `*prompt-continuations*' has in `shim.c' and `:pending' has in
;;; `rpc.lisp', and deliberately not a third mechanism: a closure parked under an
;;; integer, the integer riding into Rust, and Rust naming it back on the event.
;;; The table is what lets a tag be an integer — which is what lets it cross C as
;;; printed source — while meaning something arbitrary up here.

(defvar *scene-tags* (make-hash-table)
  "Tag -> the closure a click on that node runs.")

(defvar *scene-tag-next* 0
  "The last tag handed out. Never reset, so an id from a page that is gone names
nothing rather than naming something new — the same reason a prompt id and a
JSON-RPC request id are never reused.")

(defvar *scene-tags-shown* nil
  "True once the table describes the page currently on screen, so the next tag
allocated after it is the first of a *new* page and the old ones can go.")

(defun scene-tag (thunk)
  "Park THUNK and answer the integer a `:tag' names it by.

Usually not called by hand: `:tag (lambda () ...)' does it. Call it directly to
give several nodes one tag — a pill and the word inside it, say — so that one
closure answers for both.

The table is retired here rather than in `scene-set', and that is the whole of
the lifetime: by the time a page is installed its tags have already been
allocated, so clearing at install would throw away the very closures just built.
The first tag *after* an install is the first of the next page, and that is the
moment the last page's closures are dead."
  (when *scene-tags-shown*
    (clrhash *scene-tags*)
    (setf *scene-tags-shown* nil))
  (setf (gethash (incf *scene-tag-next*) *scene-tags*) thunk)
  *scene-tag-next*)

(defun %scene-click (tag)
  "Called by the editor when a click lands on a scene. TAG is the integer of the
nearest tagged node, or NIL for a click that landed on nothing tagged.

A tag naming no closure is *silence*: NIL arrives whenever you click the page
itself, and a stale integer arrives whenever a click races a rebuild. Neither is
a mistake anybody made, so neither is a message.

Everything else is inside a HANDLER-CASE for the same reason `%prompt-reply' and
`%rpc-event' are: a handler that signals must cost you that one click, not the
editor. Answers NIL so `eval-string' has nothing to echo over the message the
handler produced."
  (let ((k (and tag (gethash tag *scene-tags*))))
    (when k
      (handler-case (funcall k)
        (error (e) (message (format nil "scene click error: ~a" e))))))
  nil)

;;; ---------------------------------------------------------------------------
;;; The verb

(defun scene-set (&optional page)
  "Show PAGE — a node built with `block', `text', `image' or `rect' — on the live
buffer instead of its text. With no argument, take the page down.

  (scene-set (block :pad 48 (text (run \"hello\"))))
  (scene-set)                           ; and the text is back

Installing one makes the buffer read-only, because a scene is not an editing
surface, and taking it away gives the buffer back whatever it was before.
Replacing a page keeps the reader's place: a curriculum that re-renders because
one problem changed must not throw somebody on page nine back to page one.

The buffer is the live one, always. There is no scene handle and no way to name
another buffer's page — a scene lives on a *buffer*, which is what makes it
follow the document between panes and die with it.

A malformed page reports in the status line, naming what was expected and where,
and the page already up is left alone."
  ;; Every tag allocated up to here belongs to the page going up; `scene-tag'
  ;; reads this and retires them when the next page starts being built.
  (setf *scene-tags-shown* t)
  (%scene-set page))

;;; None of the builders is a command: they take `&rest' and so *look*
;;; zero-argument to the introspection `refresh-commands' does, and running one
;;; by hand builds a string nobody sees. The same lever `rpc.lisp' uses for
;;; `jobj' and `jarr', and only bound when this file is loaded from a config.
;;;
;;; `scene-set' is deliberately not in the list. Called with no argument it takes
;;; the page down, which is exactly what you want from `M-x' when a mode has left
;;; one up and you would like the text back.
(when (boundp '*hidden-commands*)
  (dolist (n '("block" "text" "run" "rect"))
    (pushnew n *hidden-commands* :test #'string=)))
