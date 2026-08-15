;;;; everforest-light — the same forest, read off paper
;;;;
;;;; Sainnhe Park's Everforest again, ported from `sainnhe/everforest', taking
;;;; the `light' / `medium' pair from upstream's `palette.md' and from the
;;;; `s:palette' dictionary in `autoload/everforest.vim'. The hexes are those
;;;; values verbatim, under upstream's own names.
;;;;
;;;;   (load-theme "everforest-light")
;;;;
;;;; This is not the dark theme inverted. Everforest publishes a separate light
;;;; accent set and the difference is not subtle: #f85552 is not #e67e80 turned
;;;; around, and the green goes from a soft sage #a7c080 to a flat olive #8da101.
;;;; The ground moves further still — #fdf6e3 is a warm paper cream rather than
;;;; a white, in the same family as gruvbox's `light0' and for the same reason,
;;;; which is that a screen-white page is the thing an eye-comfort theme is
;;;; arguing against.
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. A face left out here is a face the previous theme still owns
;;;; — its colour *and*, now that faces carry weight, its bold and its slant.
;;;; That is the whole reason the list is exhaustive and boring, and it matters
;;;; twice over for a light theme, since the face it forgets is likely to be one
;;;; last dark-theme accent shouting on a pale page.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse
;;;; these. A directory is `type', an executable `function', a symlink `link', a
;;;; staged file `string' and an unstaged one `keyword'.
;;;;
;;;; The assignments are everforest-dark's, line for line, and that file's header
;;;; gives the reasoning at length: upstream ships one set of highlight groups
;;;; and swaps only the palette under it, so the two ports differ in exactly the
;;;; values and nowhere else. In short — keywords red, functions green, types
;;;; yellow, operators orange, numbers purple, comments grey and slanted,
;;;; identifiers left in the body colour, headings walking upstream's markdown
;;;; rainbow red, orange, yellow. Strings are aqua rather than green because
;;;; upstream's tree-sitter groups say so and zemacs highlights through
;;;; tree-sitter, which has the happy side effect of keeping a call and a literal
;;;; apart. The seven port decisions — `constant' with the other literals rather
;;;; than in the body colour, `punctuation' at grey2 between upstream's split
;;;; brackets and delimiters, `markup' one step dimmer at grey0, the bar written
;;;; in the body colour, the search band on a tinted ground instead of a solid
;;;; accent, `cursor' the colour reverse video resolves to, and the gutter taking
;;;; the `ui_contrast' `high' greys — are all argued there rather than repeated
;;;; here.
;;;;
;;;; One thing is worth saying about this variant specifically, because it will
;;;; be the first thing anyone notices. Everforest light is a *low contrast*
;;;; theme, further from the WCAG numbers than anything else shipped in this
;;;; directory: on `bg0' the yellow measures about 2.1:1, the comment grey 2.6:1,
;;;; the green 2.7:1, the aqua 2.8:1, the red 3.0:1 and the blue 3.1:1. Only the
;;;; body colour, at 5.2:1, clears 4.5. Those are upstream's published values on
;;;; upstream's published ground and they are not an oversight — the whole point
;;;; of the palette is that nothing on the page is loud. Darkening the accents
;;;; would fix the numbers and produce a theme that is no longer Everforest, so
;;;; this port leaves them alone and says so instead. If you want a light theme
;;;; that shouts, `modus-operandi' is in this directory and is built for exactly
;;;; that.
;;;;
;;;; The tinted grounds are pale for the same reason, and the search band is the
;;;; palest of them: `bg_yellow' is #faedcd against a #fdf6e3 page, a warm cream
;;;; on a cool one. It is a whisper. `bg_visual' is the strong one of the set,
;;;; which is why the selection reads clearly and a search hit does not — an
;;;; ordering upstream also lives with in its own `DiffAdd'.

(in-package :zemacs)

;;; The palette, named as Everforest names it.
(let (;; Grounds
      (bg0       '(0.992 0.965 0.890))  ; #fdf6e3
      (bg1       '(0.957 0.941 0.851))  ; #f4f0d9
      (bg2       '(0.937 0.922 0.831))  ; #efebd4
      (bg4       '(0.878 0.863 0.780))  ; #e0dcc7

      ;; The tinted grounds — a hue mixed into bg0 rather than laid over it
      (bg-visual '(0.918 0.929 0.784))  ; #eaedc8
      (bg-yellow '(0.980 0.929 0.804))  ; #faedcd

      ;; Body, and the three greys that step up from it
      (fg        '(0.361 0.416 0.447))  ; #5c6a72
      (grey2     '(0.510 0.569 0.506))  ; #829181
      (grey1     '(0.576 0.624 0.569))  ; #939f91
      (grey0     '(0.651 0.690 0.627))  ; #a6b0a0

      ;; Accents — the light set, which is its own palette and not a darkening
      ;; of the dark one
      (red       '(0.973 0.333 0.322))  ; #f85552
      (orange    '(0.961 0.490 0.149))  ; #f57d26
      (yellow    '(0.875 0.627 0.000))  ; #dfa000
      (green     '(0.553 0.631 0.004))  ; #8da101
      (aqua      '(0.208 0.655 0.486))  ; #35a77c
      (blue      '(0.227 0.580 0.773))  ; #3a94c5
      (purple    '(0.875 0.412 0.729))  ; #df69ba
      )

  (apply #'set-background bg0)
  (apply #'set-foreground fg)

  ;; The 22 core faces. Anything skipped keeps the last theme's colour and
  ;; weight. The trailing name is the upstream highlight group it came from.
  (set-face "default"             fg)                 ; #5c6a72  `Normal'
  (set-face "keyword"             red)                ; #f85552  `Keyword', upright
  (set-face "function"            green)              ; #8da101  `Function', `@function'
  (set-face "type"                yellow)             ; #dfa000  `Type', `@type'
  (set-face "string"              aqua)               ; #35a77c  `@string'
  (set-face "number"              purple)             ; #df69ba  `Number', `@number'
  (set-face "comment"             grey1  :italic t)   ; #939f91  `Comment'
  (set-face "constant"            purple)             ; #df69ba  `@constant.builtin'
  (set-face "variable"            fg)                 ; #5c6a72  `@variable'
  (set-face "operator"            orange)             ; #f57d26  `Operator', `@operator'
  (set-face "punctuation"         grey2)              ; #829181  see everforest-dark
  (set-face "heading-1"           red    :bold t)     ; #f85552  `markdownH1'
  (set-face "heading-2"           orange :bold t)     ; #f57d26  `markdownH2'
  (set-face "heading-3"           yellow :bold t)     ; #dfa000  `markdownH3'
  (set-face "bold"                fg     :bold t)     ; #5c6a72  `markdownBold'
  (set-face "italic"              fg     :italic t)   ; #5c6a72  `markdownItalic'
  (set-face "link"                blue)               ; #3a94c5  `markdownUrl' -> `TSURI'
  (set-face "code"                green)              ; #8da101  `markdownCode'
  (set-face "markup"              grey0)              ; #a6b0a0  see everforest-dark
  (set-face "modeline"            bg2)                ; #efebd4  `StatusLine' background
  (set-face "modeline-inactive"   bg1)                ; #f4f0d9  `StatusLineNC' background
  (set-face "modeline-text"       fg)                 ; #5c6a72  see everforest-dark

  ;; The UI faces. Optional in general; this theme names them all.
  (set-face "region"              bg-visual)          ; #eaedc8  `Visual'
  (set-face "cursor"              fg)                 ; #5c6a72  see everforest-dark
  (set-face "current-line"        bg1)                ; #f4f0d9  `CursorLine'
  (set-face "line-number"         grey0)              ; #a6b0a0  `LineNr'
  (set-face "line-number-current" grey2)              ; #829181  `CursorLineNr'
  (set-face "divider"             bg4)                ; #e0dcc7  `WinSeparator'
  (set-face "error"               red)                ; #f85552  `ErrorMsg'
  (set-face "warning"             yellow)             ; #dfa000  `WarningMsg'
  (set-face "match"               bg-yellow)          ; #faedcd  see the header
  (set-face "popup"               bg2)                ; #efebd4  `Pmenu', `NormalFloat'
  (set-face "popup-border"        grey1)              ; #939f91  `FloatBorder'
  (set-face "accent"              green)              ; #8da101  `PmenuSel', `PmenuKind'
  )
