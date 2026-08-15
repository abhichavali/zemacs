;;;; vesper — greyscale, and one pale orange doing all the work
;;;;
;;;; Rauno Freiberg's Vesper, ported from `raunofreiberg/vesper', whose whole
;;;; theme is one file — themes/Vesper-dark-color-theme.json — small enough to
;;;; read in a sitting, and reading it is the quickest way to understand the
;;;; theme. Nearly every value in it is a grey; past those, three colours carry
;;;; meaning — #ffc799, #99ffe4 and #ff8080 — and that is the whole of it. It
;;;; distils to the eleven bindings below, seven of them grey, and the design is
;;;; the restraint. #ffc799 reads as loudly as it does because almost nothing
;;;; else on the screen is permitted to be a colour at all.
;;;;
;;;;   (load-theme "vesper")
;;;;
;;;; So the mapping below is mostly greys, and that is not an omission — it is
;;;; the port being faithful. Upstream paints keywords, operators, brackets,
;;;; regexes, escapes, language variables and inline code all in one grey,
;;;; #a0a0a0; comments in a dimmer one; your own identifiers plain white; and
;;;; it spends #ffc799 on functions, types, numbers, constants and links, and
;;;; #99ffe4 on strings. That is the list, and a port reaching for more hues
;;;; because zemacs offers thirty-four faces would be porting something else.
;;;;
;;;; Worth saying out loud, because it inverts the usual arrangement:
;;;; `keyword' is grey and `variable' is white, so the language is dimmer than
;;;; the names you gave things. Upstream means it — keyword, storage.type and
;;;; storage.modifier are all #a0a0a0 while variable is #fff — and it is the
;;;; second half of the same idea as the single accent.
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before, and on this theme a stray inherited colour is worse than
;;;; usual: it would be the only chromatic thing on the screen. Dired and the
;;;; git status buffer have no faces of their own either — a directory is
;;;; `type', an executable `function', a symlink `link', a staged file `string'
;;;; and an unstaged one `keyword'.
;;;;
;;;; What the port decides for itself:
;;;;
;;;; * `region' — upstream's editor.selectionBackground is white at 15%, which
;;;;   composites to about #333333 on the ground. zemacs faces are opaque, and
;;;;   rather than flatten the alpha this takes #232323, upstream's own opaque
;;;;   selected-item colour from the file tree: quieter, and already here.
;;;; * `current-line' and `popup' — upstream defines neither a line highlight
;;;;   nor a suggest-widget fill, and both fall through to the VS Code default.
;;;;   They take #161616, upstream's value for a raised surface (the active
;;;;   tab, the hover tooltip), which reads as a lift rather than a highlight.
;;;; * `match' and `divider' — no find-match colour upstream and no visible
;;;;   pane border either; tab.border is the ground. Both take #282828, the
;;;;   hover and tooltip-border grey, a step above the selection band so a
;;;;   search hit is not mistaken for the band under point.
;;;; * `cursor' — no editorCursor colour upstream either, but it marks where
;;;;   you are with the accent everywhere else it can: the focus ring, the
;;;;   active tab's underline, the modified-setting dot. The caret joins them.
;;;; * `line-number-current' — nor an active-line-number colour. It takes
;;;;   #a0a0a0, the status bar and icon grey, two steps up from the #505050 of
;;;;   every other digit.
;;;; * `heading-1' to `heading-3' — upstream gives every markdown heading the
;;;;   accent and no weight. zemacs paints level three and everything under it
;;;;   in one face, so three identical rungs would name nothing; they walk down
;;;;   instead, accent to white to grey, which is this theme's own gradient in
;;;;   its own order. Bold is added because in a palette this narrow there is
;;;;   no hue left to say heading with.
;;;; * `markup' — the delimiters. Upstream draws a fence brighter than the code
;;;;   it fences, #fff against #a0a0a0, which is backwards here, where markup
;;;;   is meant to stay out of the way. They take the comment grey.
;;;; * `modeline' and friends — upstream's status bar sits on the ground with
;;;;   #a0a0a0 text. zemacs writes both bars with one `modeline-text', so the
;;;;   bar has to say which window has focus: #232323 raises the active one and
;;;;   #161616 sinks the rest.
;;;;
;;;; On weight and slant: upstream italicises nothing but italic markup — not
;;;; comments, which most dark themes do — and bolds nothing but bold markup.
;;;; That is kept. Five faces are bold, one is italic, four of the five are
;;;; structure rather than syntax, and alpha is dropped twice: comments are
;;;; #8b8b8b at 58% upstream, which flattens to roughly #575757 and is too dim
;;;; to read prose in, and the selection above. Both take the flat colour.

(in-package :zemacs)

;;; The palette, named as Vesper names it.
(let (;; Grounds
      (bg        '(0.063 0.063 0.063))  ; #101010  editor.background
      (raised    '(0.086 0.086 0.086))  ; #161616  tab.activeBackground
      (selection '(0.137 0.137 0.137))  ; #232323  list.activeSelection
      (hover     '(0.157 0.157 0.157))  ; #282828  list.hoverBackground

      ;; The greys, dimmest first
      (gutter    '(0.314 0.314 0.314))  ; #505050  editorLineNumber
      (comment   '(0.545 0.545 0.545))  ; #8b8b8b  comment, flattened
      (grey      '(0.627 0.627 0.627))  ; #a0a0a0  keyword, storage, brackets
      (fg        '(1.000 1.000 1.000))  ; #ffffff  editor.foreground

      ;; The two colours, and the red that is only ever a complaint
      (primary   '(1.000 0.780 0.600))  ; #ffc799
      (secondary '(0.600 1.000 0.894))  ; #99ffe4
      (red       '(1.000 0.502 0.502))  ; #ff8080
      )

  (apply #'set-background bg)
  (apply #'set-foreground fg)

  ;; The 22 core faces. Anything skipped keeps the last theme's colour and
  ;; weight. The trailing note is the upstream scope it came from.
  (set-face "default"        fg)                    ; #ffffff  editor.foreground
  (set-face "keyword"        grey)                  ; #a0a0a0  keyword, storage.type
  (set-face "function"       primary)               ; #ffc799  entity.name.function
  (set-face "type"           primary)               ; #ffc799  support.type, support.class
  (set-face "string"         secondary)             ; #99ffe4  string
  (set-face "number"         primary)               ; #ffc799  constant.numeric
  (set-face "comment"        comment)               ; #8b8b8b  comment, upright
  (set-face "constant"       primary)               ; #ffc799  support.constant
  (set-face "variable"       fg)                    ; #ffffff  variable
  (set-face "operator"       grey)                  ; #a0a0a0  keyword.control
  (set-face "punctuation"    grey)                  ; #a0a0a0  editorBracketHighlight
  (set-face "heading-1"      primary   :bold t)     ; #ffc799  markup.heading
  (set-face "heading-2"      fg        :bold t)     ; #ffffff  see the header
  (set-face "heading-3"      grey      :bold t)     ; #a0a0a0  see the header
  (set-face "bold"           fg        :bold t)     ; #ffffff  markup.bold
  (set-face "italic"         fg        :italic t)   ; #ffffff  markup.italic
  (set-face "link"           primary)               ; #ffc799  textLink.foreground
  (set-face "code"           grey)                  ; #a0a0a0  markup.inline.raw
  (set-face "markup"         comment)               ; #8b8b8b  see the header
  (set-face "modeline"       selection)             ; #232323  bar, current window
  (set-face "modeline-inactive" raised)             ; #161616  bar, other windows
  (set-face "modeline-text"  grey      :bold t)     ; #a0a0a0  statusBar.foreground

  ;; The 12 UI faces. Optional in general — each falls back to a mix of the
  ;; ground and the body colour — but this theme names them.
  (set-face "region"         selection)             ; #232323  see the header
  (set-face "cursor"         primary)               ; #ffc799  focusBorder
  (set-face "current-line"   raised)                ; #161616  see the header
  (set-face "line-number"    gutter)                ; #505050  editorLineNumber
  (set-face "line-number-current" grey)             ; #a0a0a0  see the header
  (set-face "divider"        hover)                 ; #282828  see the header
  (set-face "error"          red)                   ; #ff8080  editorError
  (set-face "warning"        primary)               ; #ffc799  editorWarning
  (set-face "match"          hover)                 ; #282828  see the header
  (set-face "popup"          raised)                ; #161616  editorHoverWidget
  (set-face "popup-border"   hover)                 ; #282828  its border
  (set-face "accent"         primary)               ; #ffc799  list.highlightForeground
  )
