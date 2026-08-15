;;;; cyberdream — pure white on near-black, and the loudest theme in this directory
;;;;
;;;; Nothing here is muted. `scottmckendry/cyberdream.nvim' sets its body text to
;;;; #ffffff — not a warm off-white, not a desaturated blue-grey, white — drops
;;;; the ground to #16181a, and then paints every accent at full neon. Nine of
;;;; the fourteen published colours are accents, and every one of the nine is
;;;; built from the same two numbers: one channel pinned at 0xff and another at
;;;; 0x5e, with only the third free to move. That is why the palette reads as one
;;;; family of glow rather than as a set of hues that happen to be bright — they
;;;; are literally the same colour rotated. It is a screen that announces itself.
;;;; If that is not what you want, `nord' and `melange-dark' are two doors down.
;;;;
;;;; The values below are the published ones from `lua/cyberdream/colors.lua',
;;;; the `default' dark variant, copied rather than sampled. The assignments come
;;;; from `lua/cyberdream/extensions/base.lua' and `.../treesitter.lua', which are
;;;; where the theme actually lives: Keyword orange, Function blue, Type purple,
;;;; String green, Number orange, Constant pink, Identifier and Delimiter both the
;;;; body white, Operator purple, Comment grey.
;;;;
;;;; Load it from your init.lisp — these are ordinary top-level forms, so loading
;;;; the file *is* applying the theme:
;;;;
;;;;   (load-theme "cyberdream")
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. A face left out here is a face the previous theme still owns —
;;;; its colour and, worse, its weight. That matters more when the incoming theme
;;;; is this one: a leftover pastel sitting in the middle of the neon is visible
;;;; from across the room.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse these.
;;;; A directory is `type', an executable `function', a symlink `link', a staged
;;;; file `string' and an unstaged one `keyword'.
;;;;
;;;; On weight and slant: five faces are bold, two italic, and comments are
;;;; upright. That last one is upstream's own default and the one people are most
;;;; likely to expect otherwise — `italic_comments' is `false' in
;;;; `lua/cyberdream/config.lua', so the shipped theme leaves comments plain and
;;;; so does this. The italic goes where upstream puts it, on `@markup.italic',
;;;; which is blue and slanted; the bold on `@markup.strong', which is pink; and
;;;; on the three headings, which are `markdownH1' through `markdownH3' exactly —
;;;; orange, cyan, blue, all bold. Upstream's levels four through six are purple,
;;;; magenta and green, and have nowhere to go here.
;;;;
;;;; Eight mappings are not cyberdream's own:
;;;;
;;;; * The palette is thirteen colours, not fourteen. Upstream's magenta (#ff5ef1)
;;;;   is `Statement', and zemacs folds Statement into `keyword', which upstream
;;;;   paints orange via `@keyword'. The colour has nothing left to land on, and a
;;;;   binding nobody spends is a warning, so it is not bound.
;;;; * `current-line' — upstream gives `CursorLine' the same `bg_highlight' it
;;;;   gives `Visual'. zemacs draws both at once, and a stripe the exact colour of
;;;;   the selection band swallows the selection. The stripe drops to `bg_alt' —
;;;;   eight, nine and ten values off the ground, one per channel, a 1.10:1 lift
;;;;   you feel rather than see, which is what a stripe should be.
;;;; * `match' — `Search' upstream is a white bar with the text flipped to
;;;;   `bg_alt'. zemacs paints the band and leaves each token its own colour, so a
;;;;   white bar would erase the hit it is marking. A search hit therefore wears
;;;;   the same `bg_highlight' the selection wears; with three grounds in the
;;;;   palette and four bands wanted, something had to be said twice.
;;;; * `line-number' and `line-number-current' — upstream computes the gutter as
;;;;   `blend(bg_highlight, fg, 0.9)', a colour no palette file publishes. It
;;;;   works out near #4f5359, which is 2.3:1 against this ground: not a dim line
;;;;   number, a line number you have to hunt for. Both rungs move up one step on
;;;;   upstream's own ladder: the gutter takes grey, which is what upstream gives
;;;;   `CursorLineNr', and point's own line takes the white that `Cursor' and
;;;;   `Title' wear.
;;;; * `modeline' and `modeline-inactive' — upstream draws both statuslines flat
;;;;   on `bg' and tells them apart by foreground, white against grey. zemacs
;;;;   writes both bars with one `modeline-text', so the bar itself has to carry
;;;;   focus: `bg_highlight' raises the focused one clear of the buffer, `bg_alt'
;;;;   sinks the rest almost into it.
;;;; * `modeline-text' — bold, where upstream sets no weight on `StatusLine'. With
;;;;   one face for the whole bar the choice is all or nothing, and plain white on
;;;;   #3c4048 reads as one more line of the buffer.
;;;; * `popup' — `Pmenu' and `NormalFloat' both sit on `bg' upstream, i.e. a
;;;;   floating panel is the buffer with a border around it. It is raised to
;;;;   `bg_alt' here so the panel has a body; `popup-border' keeps upstream's
;;;;   `FloatBorder', which is `bg_highlight'.
;;;; * `link' — upstream has two answers and neither is spendable. The URL is
;;;;   blue and underlined (`@markup.link.url', and `markdownLinkText' says the
;;;;   same); the label, which is the part you actually read, falls through
;;;;   `@markup.link.label' to `Label' and comes out orange. zemacs draws no
;;;;   underline, and both hues are already committed three ways over — blue is
;;;;   `function', `heading-3' and `italic', orange is `keyword', `number' and
;;;;   `heading-1'. A link the colour of every function name, or of every
;;;;   keyword, is not a link. Cyan takes it — spent once here, on `heading-2',
;;;;   against three apiece for the other two — and cyan is already where
;;;;   upstream sends its other look-over-here markers, `IncSearch' and
;;;;   `RenderMarkdownHint'.
;;;;
;;;; `accent' is purple, from `CmpItemAbbrMatch' and `DashboardHeader' — the two
;;;; places upstream picks a colour for chrome rather than for code. And `code' is
;;;; pink, from `MarkviewInlineCode', which puts inline code, `constant' and
;;;; `bold' all in the same #ff5ea0. That is upstream's doing three times over,
;;;; not a shortage of colours, and it is left standing.
;;;;
;;;; `markup' — the `*' and `=' delimiters themselves — is grey, which is not a
;;;; deviation either, though it takes three files to see why: `RenderMarkdownBullet'
;;;; and `RenderMarkdownDash' are grey, and `@markup.quote' arrives at grey by way
;;;; of `Comment'. Everything cyberdream treats as furniture rather than as text
;;;; ends up in the one colour that is not neon.

(in-package :zemacs)

;;; The palette, named as cyberdream names it.
(let (;; Grounds
      (bg           '(0.086 0.094 0.102))  ; #16181a
      (bg-alt       '(0.118 0.129 0.141))  ; #1e2124
      (bg-highlight '(0.235 0.251 0.282))  ; #3c4048

      ;; Foregrounds
      (fg           '(1.000 1.000 1.000))  ; #ffffff
      (grey         '(0.482 0.518 0.588))  ; #7b8496

      ;; Accents
      (blue         '(0.369 0.631 1.000))  ; #5ea1ff
      (cyan         '(0.369 0.945 1.000))  ; #5ef1ff
      (green        '(0.369 1.000 0.424))  ; #5eff6c
      (yellow       '(0.945 1.000 0.369))  ; #f1ff5e
      (orange       '(1.000 0.741 0.369))  ; #ffbd5e
      (red          '(1.000 0.431 0.369))  ; #ff6e5e
      (pink         '(1.000 0.369 0.627))  ; #ff5ea0
      (purple       '(0.741 0.369 1.000))  ; #bd5eff
      )

  (apply #'set-background bg)
  (apply #'set-foreground fg)

  ;; All 22 of them. Anything skipped keeps the last theme's colour and weight.
  (set-face "default"             fg)              ; #ffffff  Normal
  (set-face "keyword"             orange)          ; #ffbd5e  Keyword
  (set-face "function"            blue)            ; #5ea1ff  Function
  (set-face "type"                purple)          ; #bd5eff  Type
  (set-face "string"              green)           ; #5eff6c  String
  (set-face "number"              orange)          ; #ffbd5e  Number
  (set-face "comment"             grey)            ; #7b8496  Comment, upright
  (set-face "constant"            pink)            ; #ff5ea0  Constant
  (set-face "variable"            fg)              ; #ffffff  Identifier
  (set-face "operator"            purple)          ; #bd5eff  Operator
  (set-face "punctuation"         fg)              ; #ffffff  Delimiter
  (set-face "heading-1"           orange :bold t)  ; #ffbd5e  markdownH1
  (set-face "heading-2"           cyan :bold t)    ; #5ef1ff  markdownH2
  (set-face "heading-3"           blue :bold t)    ; #5ea1ff  markdownH3
  (set-face "bold"                pink :bold t)    ; #ff5ea0  @markup.strong
  (set-face "italic"              blue :italic t)  ; #5ea1ff  @markup.italic
  (set-face "link"                cyan)            ; #5ef1ff  see the header
  (set-face "code"                pink)            ; #ff5ea0  MarkviewInlineCode
  (set-face "markup"              grey)            ; #7b8496  see the header
  (set-face "modeline"            bg-highlight)    ; #3c4048  bar, current window
  (set-face "modeline-inactive"   bg-alt)          ; #1e2124  bar, other windows
  (set-face "modeline-text"       fg :bold t)      ; #ffffff  what is written on it

  ;; The 12 UI faces. Optional in general — each falls back to a mix of the
  ;; ground and the body colour — but this theme names them.
  (set-face "region"              bg-highlight)    ; #3c4048  Visual
  (set-face "cursor"              fg)              ; #ffffff  Cursor
  (set-face "current-line"        bg-alt)          ; #1e2124  see the header
  (set-face "line-number"         grey)            ; #7b8496  see the header
  (set-face "line-number-current" fg)              ; #ffffff  see the header
  (set-face "divider"             bg-highlight)    ; #3c4048  WinSeparator
  (set-face "error"               red)             ; #ff6e5e  DiagnosticError
  (set-face "warning"             yellow)          ; #f1ff5e  DiagnosticWarn
  (set-face "match"               bg-highlight)    ; #3c4048  see the header
  (set-face "popup"               bg-alt)          ; #1e2124  see the header
  (set-face "popup-border"        bg-highlight)    ; #3c4048  FloatBorder
  (set-face "accent"              purple)          ; #bd5eff  CmpItemAbbrMatch
  )
