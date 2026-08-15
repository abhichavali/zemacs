;;;; kanagawa-wave — Hokusai's wave, and the warm dark the theme is known for
;;;;
;;;; Kanagawa is named for `The Great Wave off Kanagawa', and it is a painting
;;;; palette rather than a terminal one: the colours are named for ink, sumi,
;;;; for the seasons, for the fish and the flowers in the print, and none of
;;;; them is at full saturation except the two that mean something has gone
;;;; wrong. The values below are the published ones from `rebelot/kanagawa.nvim'
;;;; — the single `palette' table in `lua/kanagawa/colors.lua' — and the
;;;; assignments are the `wave' theme table in `lua/kanagawa/themes.lua', which
;;;; is what upstream loads when you do not ask for a variant.
;;;;
;;;; Load it from your init.lisp — these are ordinary top-level forms, so
;;;; loading the file *is* applying the theme:
;;;;
;;;;   (load-theme "kanagawa-wave")
;;;;
;;;; Upstream's own mapping is followed wherever zemacs has somewhere to put it:
;;;; strings springGreen, numbers sakuraPink, constants surimiOrange, functions
;;;; crystalBlue, types waveAqua2, keywords oniViolet, operators boatYellow2,
;;;; delimiters springViolet2, comments fujiGray. The grounds are the sumi ink
;;;; ramp, sumiInk3 for the buffer with sumiInk0 beneath it and sumiInk4,
;;;; sumiInk5 above.
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. A face left out here is a face the previous theme still
;;;; owns — its colour *and* its weight, which is the worse half of the bug —
;;;; and that is the whole reason the list is exhaustive and boring.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse
;;;; these. A directory is `type', an executable `function', a symlink `link',
;;;; a staged file `string' and an unstaged one `keyword'. So the same lines
;;;; that colour a source buffer colour those too.
;;;;
;;;; On weight and slant: upstream's defaults are `commentStyle = { italic }',
;;;; `keywordStyle = { italic }', `statementStyle = { bold }', and nothing at
;;;; all on functions or types. The two italics are ported exactly — they are
;;;; most of what the theme looks like in the wild. The bold on Statement is
;;;; not: zemacs folds statement and keyword captures onto one `keyword', and a
;;;; face cannot be selectively both, so the italic keeps it. Bold goes where
;;;; upstream also puts it, on headings — its `Title' is `syn.fun' bold — and
;;;; on the modeline for the reason below.
;;;;
;;;; Six mappings are this port's own:
;;;;
;;;; * `heading-2' and `heading-3' — upstream has one heading colour, not three:
;;;;   `@markup.heading' links to `Function' at every level. Three identical
;;;;   levels name nothing, so level 1 keeps crystalBlue and the ramp continues
;;;;   into carpYellow and waveAqua2. carpYellow is otherwise unspent here; it
;;;;   is upstream's `syn.identifier', which zemacs has no separate face for.
;;;; * `modeline' and `modeline-inactive' — upstream draws both statuslines on
;;;;   the same dark ground and tells them apart by foreground, `fg_dim' against
;;;;   `nontext'. zemacs writes both bars with one `modeline-text', so the
;;;;   difference has to live in the bar itself: the focused one rises to
;;;;   sumiInk5 and the rest sink to sumiInk0.
;;;; * `modeline-text' — bold. Emacs bolds the parts of the mode line that carry
;;;;   information and leaves the rest plain; with a single face for the whole
;;;;   bar the choice is all or nothing. The colour is upstream's `fg_dim',
;;;;   oldWhite, which is the statusline foreground there too.
;;;; * `current-line' — upstream's `CursorLine' is `bg_p2', sumiInk5, which in
;;;;   Neovim sits under a narrow window and here would be a full-width band
;;;;   exactly as bright as the modeline. It steps down one to `bg_p1',
;;;;   sumiInk4 — still upstream's own ink, and the ground kanagawa gives
;;;;   `ColorColumn' and folded text.
;;;; * `markup' — the `*' and `=' delimiters. Upstream has no face for them, and
;;;;   inheriting the comment colour would make the asterisks exactly as loud as
;;;;   the word between them. They take sumiInk6, upstream's `nontext', which is
;;;;   what that colour is for: the furniture, not the text.
;;;; * `link' — kanagawa underlines URLs and zemacs does not underline, so the
;;;;   face has to carry a hue. It takes springBlue, upstream's `special1'.
;;;;
;;;; `variable' is fujiWhite, and that is upstream's own: `syn.variable' in the
;;;; wave table is explicitly nothing, so a plain identifier stays in the body
;;;; colour and only the ones that are something in particular get painted. The
;;;; theme is warm because of what it marks, not because it marks everything.
;;;;
;;;; One value is worth flagging as surprising rather than wrong:
;;;; `line-number-current' is roninYellow, because upstream's `CursorLineNr' is
;;;; `diag.warning' bold. It looks like a mistake in a palette listing and is
;;;; not — it is how you find your line in a screen of muted paint.

(in-package :zemacs)

;;; The palette, named as kanagawa names it.
(let (;; The sumi ink ramp — the grounds
      (sumi-ink0      '(0.086 0.086 0.114))  ; #16161d
      (sumi-ink3      '(0.122 0.122 0.157))  ; #1f1f28
      (sumi-ink4      '(0.165 0.165 0.216))  ; #2a2a37
      (sumi-ink5      '(0.212 0.212 0.275))  ; #363646
      (sumi-ink6      '(0.329 0.329 0.427))  ; #54546d

      ;; The wave blues — the bands drawn behind text
      (wave-blue1     '(0.133 0.196 0.286))  ; #223249
      (wave-blue2     '(0.176 0.310 0.404))  ; #2d4f67

      ;; Fuji — the foregrounds
      (fuji-white     '(0.863 0.843 0.729))  ; #dcd7ba
      (old-white      '(0.784 0.753 0.576))  ; #c8c093
      (fuji-gray      '(0.447 0.443 0.412))  ; #727169

      ;; Spring and crystal — the cool accents
      (oni-violet     '(0.584 0.498 0.722))  ; #957fb8
      (crystal-blue   '(0.494 0.612 0.847))  ; #7e9cd8
      (spring-violet2 '(0.612 0.671 0.792))  ; #9cabca
      (spring-blue    '(0.498 0.706 0.792))  ; #7fb4ca
      (wave-aqua2     '(0.478 0.659 0.624))  ; #7aa89f
      (spring-green   '(0.596 0.733 0.424))  ; #98bb6c

      ;; Sakura, surimi, boat and carp — the warm ones
      (sakura-pink    '(0.824 0.494 0.600))  ; #d27e99
      (surimi-orange  '(1.000 0.627 0.400))  ; #ffa066
      (boat-yellow2   '(0.753 0.639 0.431))  ; #c0a36e
      (carp-yellow    '(0.902 0.765 0.518))  ; #e6c384

      ;; The two that mean something is wrong
      (samurai-red    '(0.910 0.141 0.141))  ; #e82424
      (ronin-yellow   '(1.000 0.620 0.231))  ; #ff9e3b
      )

  (apply #'set-background sumi-ink3)
  (apply #'set-foreground fuji-white)

  ;; The 22 core faces. Anything skipped keeps the last theme's colour and
  ;; weight. The trailing name is the `themes.lua' field the mapping came from.
  (set-face "default"             fuji-white)                    ; #dcd7ba  ui.fg
  (set-face "keyword"             oni-violet     :italic t)      ; #957fb8  syn.keyword
  (set-face "function"            crystal-blue)                  ; #7e9cd8  syn.fun
  (set-face "type"                wave-aqua2)                    ; #7aa89f  syn.type
  (set-face "string"              spring-green)                  ; #98bb6c  syn.string
  (set-face "number"              sakura-pink)                   ; #d27e99  syn.number
  (set-face "comment"             fuji-gray      :italic t)      ; #727169  syn.comment
  (set-face "constant"            surimi-orange)                 ; #ffa066  syn.constant
  (set-face "variable"            fuji-white)                    ; #dcd7ba  syn.variable is none
  (set-face "operator"            boat-yellow2)                  ; #c0a36e  syn.operator
  (set-face "punctuation"         spring-violet2)                ; #9cabca  syn.punct
  (set-face "heading-1"           crystal-blue   :bold t)        ; #7e9cd8  Title
  (set-face "heading-2"           carp-yellow    :bold t)        ; #e6c384  see the header
  (set-face "heading-3"           wave-aqua2     :bold t)        ; #7aa89f  see the header
  (set-face "bold"                fuji-white     :bold t)        ; #dcd7ba  weight only
  (set-face "italic"              fuji-white     :italic t)      ; #dcd7ba  slant only
  (set-face "link"                spring-blue)                   ; #7fb4ca  syn.special1
  (set-face "code"                spring-green)                  ; #98bb6c  markup.raw is String
  (set-face "markup"              sumi-ink6)                     ; #54546d  see the header
  (set-face "modeline"            sumi-ink5)                     ; #363646  bar, current window
  (set-face "modeline-inactive"   sumi-ink0)                     ; #16161d  bar, other windows
  (set-face "modeline-text"       old-white      :bold t)        ; #c8c093  ui.fg_dim

  ;; The UI faces. Optional in general — each falls back to a mix of the ground
  ;; and the body colour — but this theme names them.
  (set-face "region"              wave-blue1)                    ; #223249  ui.bg_visual
  (set-face "cursor"              fuji-white)                    ; #dcd7ba  Cursor, reversed
  (set-face "current-line"        sumi-ink4)                     ; #2a2a37  ui.bg_p1
  (set-face "line-number"         sumi-ink6)                     ; #54546d  ui.nontext
  (set-face "line-number-current" ronin-yellow)                  ; #ff9e3b  CursorLineNr
  (set-face "divider"             sumi-ink0)                     ; #16161d  WinSeparator
  (set-face "error"               samurai-red)                   ; #e82424  diag.error
  (set-face "warning"             ronin-yellow)                  ; #ff9e3b  diag.warning
  (set-face "match"               wave-blue2)                    ; #2d4f67  ui.bg_search
  (set-face "popup"               sumi-ink0)                     ; #16161d  ui.float.bg
  (set-face "popup-border"        sumi-ink6)                     ; #54546d  ui.float.fg_border
  (set-face "accent"              crystal-blue)                  ; #7e9cd8  syn.fun
  )
