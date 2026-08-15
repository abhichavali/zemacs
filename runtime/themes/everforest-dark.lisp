;;;; everforest-dark — a green-based palette that is trying not to tire you out
;;;;
;;;; Sainnhe Park's Everforest, ported from `sainnhe/everforest'. The values are
;;;; the `dark' / `medium' pair exactly as it stands in upstream's `palette.md'
;;;; and in the `s:palette' dictionary built by `autoload/everforest.vim' — not
;;;; approximations, and not the hard variant's. That last distinction matters
;;;; more than it looks: the three background steps differ in more than `bg0',
;;;; and the selection band is #4c3743 on hard, #543a48 on medium and #5c3f4f on
;;;; soft. Medium is what is below.
;;;;
;;;; What makes this Everforest rather than one more dark theme is the ground.
;;;; #2d353b is a desaturated blue-green, not the blue-black nearly every other
;;;; dark theme reaches for; the body colour is a warm cream rather than a white;
;;;; and every accent is pulled well back from full saturation. Upstream's own
;;;; word for the goal is "protect developers' eyes", and the contrast is
;;;; moderate on purpose. A port that raises it has ported something else.
;;;;
;;;; Load it from your init.lisp — these are ordinary top-level forms, so loading
;;;; the file *is* applying the theme:
;;;;
;;;;   (load-theme "everforest-dark")
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. A face left out here is a face the previous theme still owns
;;;; — its colour *and*, now that faces carry weight, its bold and its slant.
;;;; That is the whole reason the list is exhaustive and boring.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse
;;;; these. A directory is `type', an executable `function', a symlink `link', a
;;;; staged file `string' and an unstaged one `keyword'.
;;;;
;;;; Upstream has two syntax mappings and they disagree in one place. The vim
;;;; syntax groups give `String' green, the same green as `Function'; the
;;;; tree-sitter ones give `@string' aqua and leave green to `@function'. zemacs
;;;; highlights through tree-sitter, so the tree-sitter answer is the one taken
;;;; here — and it is the better of the two anyway, since collapsing a call and a
;;;; literal onto one hue is exactly the distinction a code buffer needs.
;;;; Everything else is upstream's without argument: keywords red, types yellow,
;;;; operators orange, numbers purple, comments grey and slanted, identifiers
;;;; left in the body colour.
;;;;
;;;; On weight and slant: one face is italic and five are bold, which is all
;;;; upstream has. `everforest_enable_italic' ships 0, so keywords are upright
;;;; here as they are there; `everforest_disable_italic_comment' ships 0, so
;;;; comments are slanted. The bold is upstream's markdown headings, which are
;;;; `markdownH1' through `markdownH6' walking red, orange, yellow, green, blue,
;;;; purple — zemacs stops at three, so it gets the first three.
;;;;
;;;; Seven mappings are the port's own:
;;;;
;;;; * `constant' — upstream gives three answers. `@constant' is the body colour,
;;;;   vim's `Constant' is aqua, `@constant.builtin' is purple. Aqua is spent on
;;;;   literals already and the body colour would make a dired mark invisible, so
;;;;   purple it is, which also puts it with `number' where a literal belongs.
;;;; * `punctuation' — upstream splits it: brackets take the body colour and
;;;;   delimiters take grey1, the comment grey. zemacs has one face for both, and
;;;;   grey2 is the step between them — dim enough that a Lisp buffer's parens
;;;;   recede, bright enough that they do not read as commented out.
;;;; * `markup' — the `*' and `=' delimiters. Upstream paints them grey1 too,
;;;;   which would make the marker exactly as loud as the comment beside it; they
;;;;   step one further down to grey0, which is what upstream gives `Conceal'
;;;;   once `everforest_ui_contrast' is `high' — the same step the gutter takes
;;;;   in the last bullet below, and for the same reason.
;;;; * `modeline-text' — upstream's `StatusLine' is grey1 on bg2, which is 2.9:1
;;;;   and fine in vim, where the statusline is mostly separators. zemacs writes
;;;;   the buffer name, the position and the mode with this one face and nothing
;;;;   else, so it takes the body colour. No bold: upstream's bar has no weight
;;;;   on it, and this theme is not the place to invent some. The bar itself
;;;;   keeps upstream's own pair, bg2 focused and bg1 not.
;;;; * `match' — upstream's `Search' is bg0 written on solid green. zemacs draws
;;;;   a band and does not repaint the text on top of it, so a solid accent would
;;;;   be a hit you cannot read. It takes the tinted `bg_yellow' instead, which
;;;;   is upstream's `Substitute' hue in ground form and stays clear of the
;;;;   selection band on both variants of this theme.
;;;; * `cursor' — `everforest_cursor' ships `auto', and `auto' is the one branch
;;;;   that names no colour at all: it sets `Cursor' to a bare `reverse' and lets
;;;;   the terminal work it out. The body colour is what reverse video on this
;;;;   ground comes out as.
;;;; * `line-number' and `line-number-current' — `everforest_ui_contrast' ships
;;;;   `low', which puts the gutter on bg5. That is under 2:1 on the light
;;;;   variant's ground and not much better here, and zemacs has no such option
;;;;   to turn up, so both files take the `high' pair: grey0 dim, grey2 bright.
;;;;
;;;; One collision is upstream's and is left alone. `Directory' is green and
;;;; `Type' is yellow, and dired's directories come out of the same face as a
;;;; type name here, so they are yellow. Naming a colour is worth more than
;;;; matching a file manager.

(in-package :zemacs)

;;; The palette, named as Everforest names it.
(let (;; Grounds
      (bg0       '(0.176 0.208 0.231))  ; #2d353b
      (bg1       '(0.204 0.247 0.267))  ; #343f44
      (bg2       '(0.239 0.282 0.302))  ; #3d484d
      (bg4       '(0.310 0.345 0.369))  ; #4f585e

      ;; The tinted grounds — a hue mixed into bg0 rather than laid over it
      (bg-visual '(0.329 0.227 0.282))  ; #543a48
      (bg-yellow '(0.302 0.298 0.263))  ; #4d4c43

      ;; Body, and the three greys that step down from it
      (fg        '(0.827 0.776 0.667))  ; #d3c6aa
      (grey2     '(0.616 0.663 0.627))  ; #9da9a0
      (grey1     '(0.522 0.573 0.537))  ; #859289
      (grey0     '(0.478 0.518 0.471))  ; #7a8478

      ;; Accents
      (red       '(0.902 0.494 0.502))  ; #e67e80
      (orange    '(0.902 0.596 0.459))  ; #e69875
      (yellow    '(0.859 0.737 0.498))  ; #dbbc7f
      (green     '(0.655 0.753 0.502))  ; #a7c080
      (aqua      '(0.514 0.753 0.573))  ; #83c092
      (blue      '(0.498 0.733 0.702))  ; #7fbbb3
      (purple    '(0.839 0.600 0.714))  ; #d699b6
      )

  (apply #'set-background bg0)
  (apply #'set-foreground fg)

  ;; The 22 core faces. Anything skipped keeps the last theme's colour and
  ;; weight. The trailing name is the upstream highlight group it came from.
  (set-face "default"             fg)                 ; #d3c6aa  `Normal'
  (set-face "keyword"             red)                ; #e67e80  `Keyword', upright
  (set-face "function"            green)              ; #a7c080  `Function', `@function'
  (set-face "type"                yellow)             ; #dbbc7f  `Type', `@type'
  (set-face "string"              aqua)               ; #83c092  `@string'
  (set-face "number"              purple)             ; #d699b6  `Number', `@number'
  (set-face "comment"             grey1  :italic t)   ; #859289  `Comment'
  (set-face "constant"            purple)             ; #d699b6  `@constant.builtin'
  (set-face "variable"            fg)                 ; #d3c6aa  `@variable'
  (set-face "operator"            orange)             ; #e69875  `Operator', `@operator'
  (set-face "punctuation"         grey2)              ; #9da9a0  see the header
  (set-face "heading-1"           red    :bold t)     ; #e67e80  `markdownH1'
  (set-face "heading-2"           orange :bold t)     ; #e69875  `markdownH2'
  (set-face "heading-3"           yellow :bold t)     ; #dbbc7f  `markdownH3'
  (set-face "bold"                fg     :bold t)     ; #d3c6aa  `markdownBold'
  (set-face "italic"              fg     :italic t)   ; #d3c6aa  `markdownItalic'
  (set-face "link"                blue)               ; #7fbbb3  `markdownUrl' -> `TSURI'
  (set-face "code"                green)              ; #a7c080  `markdownCode'
  (set-face "markup"              grey0)              ; #7a8478  see the header
  (set-face "modeline"            bg2)                ; #3d484d  `StatusLine' background
  (set-face "modeline-inactive"   bg1)                ; #343f44  `StatusLineNC' background
  (set-face "modeline-text"       fg)                 ; #d3c6aa  see the header

  ;; The UI faces. Optional in general; this theme names them all.
  (set-face "region"              bg-visual)          ; #543a48  `Visual'
  (set-face "cursor"              fg)                 ; #d3c6aa  see the header
  (set-face "current-line"        bg1)                ; #343f44  `CursorLine'
  (set-face "line-number"         grey0)              ; #7a8478  `LineNr'
  (set-face "line-number-current" grey2)              ; #9da9a0  `CursorLineNr'
  (set-face "divider"             bg4)                ; #4f585e  `WinSeparator'
  (set-face "error"               red)                ; #e67e80  `ErrorMsg'
  (set-face "warning"             yellow)             ; #dbbc7f  `WarningMsg'
  (set-face "match"               bg-yellow)          ; #4d4c43  see the header
  (set-face "popup"               bg2)                ; #3d484d  `Pmenu', `NormalFloat'
  (set-face "popup-border"        grey1)              ; #859289  `FloatBorder'
  (set-face "accent"              green)              ; #a7c080  `PmenuSel', `PmenuKind'
  )
