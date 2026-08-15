;;;; oxocarbon — IBM Carbon at full volume, on a ground that is nearly black
;;;;
;;;; The reference implementation is `nyoom-engineering/oxocarbon.nvim', written
;;;; in Fennel, and the palette below is its dark variant. Oxocarbon is a subset
;;;; of IBM's Carbon design system: an industrial grey ramp under accents taken
;;;; from Carbon's blue family and the edges of it, which is why there is not one
;;;; warm colour in the file — even the pink has blue in it.
;;;;
;;;;   (load-theme "oxocarbon")
;;;;
;;;; The greys needed checking, because upstream no longer ships base01 to base05
;;;; as literals: it blends base00 towards base06 in HSLuv at 8.5%, 18%, 30%, 82%
;;;; and 95%. Running that blend returns #262626, #393939 and #525252 for base01
;;;; to base03 — Carbon's Gray 90, 80 and 70, and the published values to the
;;;; byte — but #d0d0d0 and #f2f2f2 for base04 and base05, where the published
;;;; palette has #dde1e6 and #f2f4f8, Carbon's Gray 20 and Gray 10, whose trace
;;;; of blue a neutral blend cannot keep. Tinted Theming's base16 scheme
;;;; `oxocarbon-dark', credited to shaunsingh and IBM, keeps the published pair,
;;;; and so does this file: body text with a little blue in it is most of what
;;;; the theme feels like. Everything from base07 up is a literal upstream and
;;;; is copied unchanged. base05, base06 and base13 are not bound at all —
;;;; upstream spends them on a float's foreground, a search hit's foreground and
;;;; `Todo', and zemacs has a face for none of the three.
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. A face left out is a face the previous theme still owns — its
;;;; colour and, since faces carry weight too, its bold. Dired and the git status
;;;; buffer have no faces of their own either: a directory is `type', an
;;;; executable `function', a symlink `link', a staged file `string' and an
;;;; unstaged one `keyword'.
;;;;
;;;; Repeated colours are upstream's doing rather than an oversight. base09 is
;;;; `keyword', `type' and `operator' at once because `Keyword', `Type',
;;;; `Statement', `Structure' and `Operator' all resolve to it; base14 is
;;;; `string', `constant' and `code' because `String', `@constant' and
;;;; `markdownCode' do. Oxocarbon is centred on blue and spends its other hues
;;;; sparingly, and flattening that out would be a different theme.
;;;;
;;;; On weight and slant: upstream italicises comments, bolds exactly one
;;;; font-lock group — `@function' — and leaves everything else upright and
;;;; plain. Both are kept. The bold on the headings and on the bar is this
;;;; port's, for the reasons below.
;;;;
;;;; Six mappings are not oxocarbon's own:
;;;;
;;;; * `function' — upstream has two answers. The legacy `Function' group is
;;;;   base08 and the tree-sitter `@function' is base12 and bold. zemacs colours
;;;;   from tree-sitter captures, so the tree-sitter answer wins, bold included.
;;;; * `heading-1/2/3' — upstream links every heading at every level, in
;;;;   markdown, vimwiki and asciidoc alike, to one group painted base10. Three
;;;;   identical levels would name nothing, so level one keeps base10 and the
;;;;   ramp continues into base12 and base11 — the pink next to it in the
;;;;   palette, then the blue that Tinted Theming's scheme files under base0D.
;;;; * `link' — upstream paints a URL base14, which is already `string' and
;;;;   `code'. It takes base07 instead, the teal oxocarbon reserves for builtin
;;;;   constants and for added lines in a diff, so that a link in an org file is
;;;;   not the colour of the verbatim text beside it.
;;;; * `markup' — upstream draws a heading's `*' markers in the heading's own
;;;;   colour. zemacs dims delimiters on purpose, so they take the comment grey.
;;;; * `modeline' and `modeline-inactive' — upstream draws the focused bar on
;;;;   base00, the buffer ground itself, and sinks the unfocused ones to base01,
;;;;   telling them apart by foreground. zemacs writes every bar with one
;;;;   `modeline-text', so the bar has to carry the difference: base02 raises the
;;;;   focused one and base01 leaves the rest where upstream had them. That one
;;;;   face is also why it is bold — with a single face, emphasis is all or none.
;;;; * `popup-border' — upstream draws a float's border in the float's own fill,
;;;;   i.e. invisibly. base02 gives the panel an edge.
;;;;
;;;; `match' is upstream's, with a caveat: upstream repaints the text on a hit
;;;; base01 as well, and zemacs colours only the band under it.

(in-package :zemacs)

;;; The palette, numbered as oxocarbon numbers it. Note that the numbering runs
;;; base00 to base15 in decimal, which is upstream's own scheme and not base16's
;;; hexadecimal base00 to base0F.
(let (;; The Carbon greys
      (base00 '(0.086 0.086 0.086))  ; #161616  Gray 100
      (base01 '(0.149 0.149 0.149))  ; #262626  Gray 90
      (base02 '(0.224 0.224 0.224))  ; #393939  Gray 80
      (base03 '(0.322 0.322 0.322))  ; #525252  Gray 70
      (base04 '(0.867 0.882 0.902))  ; #dde1e6  Gray 20

      ;; The accents
      (base07 '(0.031 0.741 0.729))  ; #08bdba
      (base08 '(0.239 0.859 0.851))  ; #3ddbd9
      (base09 '(0.471 0.663 1.000))  ; #78a9ff
      (base10 '(0.933 0.325 0.588))  ; #ee5396
      (base11 '(0.200 0.694 1.000))  ; #33b1ff
      (base12 '(1.000 0.494 0.714))  ; #ff7eb6
      (base14 '(0.745 0.584 1.000))  ; #be95ff
      (base15 '(0.510 0.812 1.000))  ; #82cfff
      )

  (apply #'set-background base00)
  (apply #'set-foreground base04)

  ;; The 22 core faces. Anything skipped keeps the last theme's colour and weight.
  ;; The trailing name is the upstream group the mapping came from.
  (set-face "default"           base04)               ; #dde1e6  Normal
  (set-face "keyword"           base09)               ; #78a9ff  @keyword, Statement
  (set-face "function"          base12 :bold t)       ; #ff7eb6  @function
  (set-face "type"              base09)               ; #78a9ff  Type, Structure
  (set-face "string"            base14)               ; #be95ff  String, Character
  (set-face "number"            base15)               ; #82cfff  Number, Float
  (set-face "comment"           base03 :italic t)     ; #525252  Comment
  (set-face "constant"          base14)               ; #be95ff  @constant
  (set-face "variable"          base04)               ; #dde1e6  @variable
  (set-face "operator"          base09)               ; #78a9ff  Operator
  (set-face "punctuation"       base08)               ; #3ddbd9  @punctuation.bracket
  (set-face "heading-1"         base10 :bold t)       ; #ee5396  markdownH1
  (set-face "heading-2"         base12 :bold t)       ; #ff7eb6  see the header
  (set-face "heading-3"         base11 :bold t)       ; #33b1ff  see the header
  (set-face "bold"              base04 :bold t)       ; #dde1e6  Bold, weight only
  (set-face "italic"            base04 :italic t)     ; #dde1e6  Italic, slant only
  (set-face "link"              base07)               ; #08bdba  see the header
  (set-face "code"              base14)               ; #be95ff  markdownCode
  (set-face "markup"            base03)               ; #525252  see the header
  (set-face "modeline"          base02)               ; #393939  bar, current window
  (set-face "modeline-inactive" base01)               ; #262626  bar, other windows
  (set-face "modeline-text"     base04 :bold t)       ; #dde1e6  what is written on it

  ;; The UI faces. Each of these would fall back to a mix of the ground and the
  ;; body colour if it were left out; oxocarbon names all twelve.
  (set-face "region"             base02)              ; #393939  Visual
  (set-face "cursor"             base04)              ; #dde1e6  Cursor, the block
  (set-face "current-line"       base01)              ; #262626  CursorLine
  (set-face "line-number"        base03)              ; #525252  LineNr
  (set-face "line-number-current" base04)             ; #dde1e6  CursorLineNr
  (set-face "divider"            base01)              ; #262626  WinSeparator
  (set-face "error"              base10)              ; #ee5396  DiagnosticError
  (set-face "warning"            base14)              ; #be95ff  DiagnosticWarn
  (set-face "match"              base08)              ; #3ddbd9  Search, the band
  (set-face "popup"              base01)              ; #262626  Pmenu
  (set-face "popup-border"       base02)              ; #393939  see the header
  (set-face "accent"             base08)              ; #3ddbd9  PmenuSel, Directory
  )
