;;;; kanagawa-dragon — the same painting, left out in the weather
;;;;
;;;; Kanagawa's second dark variant, added long after the first and drawn from
;;;; the `dragon*' range of the one shared `palette' table in
;;;; `rebelot/kanagawa.nvim' — `lua/kanagawa/colors.lua' — with the assignments
;;;; taken from the `dragon' theme table in `lua/kanagawa/themes.lua'. Where
;;;; `kanagawa-wave' is ink and seasonal colour, this is charcoal and ash: the
;;;; ground drops to dragonBlack3 and every accent loses most of its chroma, so
;;;; that violet, blue and teal end up within a few points of each other and the
;;;; theme is read by shape and by warmth rather than by hue. It is the variant
;;;; for a bright room and a long day.
;;;;
;;;; Load it from your init.lisp — these are ordinary top-level forms, so
;;;; loading the file *is* applying the theme:
;;;;
;;;;   (load-theme "kanagawa-dragon")
;;;;
;;;; Upstream's own mapping is followed wherever zemacs has somewhere to put it:
;;;; strings dragonGreen2, numbers dragonPink, constants dragonOrange, functions
;;;; dragonBlue2, types dragonAqua, keywords dragonViolet, operators dragonRed,
;;;; delimiters dragonGray2, comments dragonAsh.
;;;;
;;;; Six of the colours below are not dragon colours at all. The palette is one
;;;; table and the variants only choose from it, and dragon reaches out of its
;;;; own range six times: waveBlue1 for the selection, waveBlue2 for a search
;;;; hit, sumiInk6 for a float border, oldWhite for the dimmed foreground, and
;;;; samuraiRed and roninYellow for the diagnostics. They are upstream's, not
;;;; this port's, and they are the reason a selection in this theme is blue when
;;;; nothing else is.
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. A face left out here is a face the previous theme still
;;;; owns — its colour *and* its weight, which is the worse half of the bug —
;;;; and it matters more here than anywhere: load dragon after a loud theme,
;;;; miss a face, and the one thing left saturated is the one thing this variant
;;;; exists to avoid.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse
;;;; these. A directory is `type', an executable `function', a symlink `link',
;;;; a staged file `string' and an unstaged one `keyword'.
;;;;
;;;; On weight and slant: upstream's defaults are `commentStyle = { italic }',
;;;; `keywordStyle = { italic }', `statementStyle = { bold }', and nothing on
;;;; functions or types. The two italics are ported exactly. The bold on
;;;; Statement is not: zemacs folds statement and keyword captures onto one
;;;; `keyword', and a face cannot be selectively both, so the italic keeps it.
;;;; Bold goes where upstream also puts it, on headings — its `Title' is
;;;; `syn.fun' bold — and on the modeline for the reason below.
;;;;
;;;; Six mappings are this port's own:
;;;;
;;;; * `heading-2' and `heading-3' — upstream has one heading colour, not three:
;;;;   `@markup.heading' links to `Function' at every level. Level 1 keeps
;;;;   dragonBlue2 and the ramp continues into dragonYellow and dragonAqua.
;;;;   dragonYellow is otherwise unspent; it is upstream's `syn.identifier',
;;;;   which zemacs has no separate face for.
;;;; * `modeline' and `modeline-inactive' — upstream draws both statuslines on
;;;;   the same dark ground and separates them by foreground. zemacs writes both
;;;;   bars with one `modeline-text', so the difference has to live in the bar:
;;;;   the focused one rises to dragonBlack5, the rest sink to dragonBlack0.
;;;; * `modeline-text' — bold, and oldWhite, which is upstream's `fg_dim' and
;;;;   its statusline foreground. Emacs bolds the parts of the mode line that
;;;;   carry information and leaves the rest plain; with one face for the whole
;;;;   bar the choice is all or nothing.
;;;; * `current-line' — upstream's `CursorLine' is `bg_p2', dragonBlack5, which
;;;;   here would be a full-width band exactly as bright as the modeline. It
;;;;   steps down one to `bg_p1', dragonBlack4 — still upstream's own black, and
;;;;   the ground kanagawa gives the gutter and folded text.
;;;; * `markup' — the `*' and `=' delimiters, which upstream has no face for.
;;;;   Inheriting the comment colour would make the asterisks as loud as the word
;;;;   between them, so they take dragonBlack6, upstream's `nontext': furniture
;;;;   rather than text.
;;;; * `link' — kanagawa underlines URLs and zemacs does not underline, so the
;;;;   face carries a hue instead: dragonTeal, upstream's `special1'.
;;;;
;;;; `variable' is dragonWhite, which is upstream's own — `syn.variable' in the
;;;; dragon table is explicitly nothing, so a plain identifier stays in the body
;;;; colour and only the ones that are something in particular get painted.
;;;;
;;;; And `line-number-current' is roninYellow, the one hot colour on the screen,
;;;; because upstream's `CursorLineNr' is `diag.warning' bold. In a palette this
;;;; quiet it is the only way to find your own line at a glance.

(in-package :zemacs)

;;; The palette, named as kanagawa names it.
(let (;; The dragon blacks — the grounds
      (dragon-black0  '(0.051 0.047 0.047))  ; #0d0c0c
      (dragon-black3  '(0.094 0.086 0.086))  ; #181616
      (dragon-black4  '(0.157 0.153 0.153))  ; #282727
      (dragon-black5  '(0.224 0.220 0.212))  ; #393836
      (dragon-black6  '(0.384 0.369 0.353))  ; #625e5a

      ;; Borrowed from outside the dragon range — see the header
      (sumi-ink6      '(0.329 0.329 0.427))  ; #54546d
      (wave-blue1     '(0.133 0.196 0.286))  ; #223249
      (wave-blue2     '(0.176 0.310 0.404))  ; #2d4f67
      (old-white      '(0.784 0.753 0.576))  ; #c8c093

      ;; The foregrounds
      (dragon-white   '(0.773 0.788 0.773))  ; #c5c9c5
      (dragon-gray2   '(0.620 0.608 0.576))  ; #9e9b93
      (dragon-ash     '(0.451 0.486 0.451))  ; #737c73

      ;; The cool accents, such as they are
      (dragon-violet  '(0.537 0.573 0.655))  ; #8992a7
      (dragon-teal    '(0.580 0.624 0.710))  ; #949fb5
      (dragon-blue2   '(0.545 0.643 0.690))  ; #8ba4b0
      (dragon-aqua    '(0.557 0.643 0.635))  ; #8ea4a2
      (dragon-green2  '(0.541 0.604 0.482))  ; #8a9a7b

      ;; The warm ones
      (dragon-pink    '(0.635 0.573 0.639))  ; #a292a3
      (dragon-orange  '(0.714 0.573 0.482))  ; #b6927b
      (dragon-yellow  '(0.769 0.698 0.541))  ; #c4b28a
      (dragon-red     '(0.769 0.455 0.431))  ; #c4746e

      ;; The two that mean something is wrong
      (samurai-red    '(0.910 0.141 0.141))  ; #e82424
      (ronin-yellow   '(1.000 0.620 0.231))  ; #ff9e3b
      )

  (apply #'set-background dragon-black3)
  (apply #'set-foreground dragon-white)

  ;; The 22 core faces. Anything skipped keeps the last theme's colour and
  ;; weight. The trailing name is the `themes.lua' field the mapping came from.
  (set-face "default"             dragon-white)                  ; #c5c9c5  ui.fg
  (set-face "keyword"             dragon-violet  :italic t)      ; #8992a7  syn.keyword
  (set-face "function"            dragon-blue2)                  ; #8ba4b0  syn.fun
  (set-face "type"                dragon-aqua)                   ; #8ea4a2  syn.type
  (set-face "string"              dragon-green2)                 ; #8a9a7b  syn.string
  (set-face "number"              dragon-pink)                   ; #a292a3  syn.number
  (set-face "comment"             dragon-ash     :italic t)      ; #737c73  syn.comment
  (set-face "constant"            dragon-orange)                 ; #b6927b  syn.constant
  (set-face "variable"            dragon-white)                  ; #c5c9c5  syn.variable is none
  (set-face "operator"            dragon-red)                    ; #c4746e  syn.operator
  (set-face "punctuation"         dragon-gray2)                  ; #9e9b93  syn.punct
  (set-face "heading-1"           dragon-blue2   :bold t)        ; #8ba4b0  Title
  (set-face "heading-2"           dragon-yellow  :bold t)        ; #c4b28a  see the header
  (set-face "heading-3"           dragon-aqua    :bold t)        ; #8ea4a2  see the header
  (set-face "bold"                dragon-white   :bold t)        ; #c5c9c5  weight only
  (set-face "italic"              dragon-white   :italic t)      ; #c5c9c5  slant only
  (set-face "link"                dragon-teal)                   ; #949fb5  syn.special1
  (set-face "code"                dragon-green2)                 ; #8a9a7b  markup.raw is String
  (set-face "markup"              dragon-black6)                 ; #625e5a  see the header
  (set-face "modeline"            dragon-black5)                 ; #393836  bar, current window
  (set-face "modeline-inactive"   dragon-black0)                 ; #0d0c0c  bar, other windows
  (set-face "modeline-text"       old-white      :bold t)        ; #c8c093  ui.fg_dim

  ;; The UI faces. Optional in general — each falls back to a mix of the ground
  ;; and the body colour — but this theme names them.
  (set-face "region"              wave-blue1)                    ; #223249  ui.bg_visual
  (set-face "cursor"              dragon-white)                  ; #c5c9c5  Cursor, reversed
  (set-face "current-line"        dragon-black4)                 ; #282727  ui.bg_p1
  (set-face "line-number"         dragon-black6)                 ; #625e5a  ui.nontext
  (set-face "line-number-current" ronin-yellow)                  ; #ff9e3b  CursorLineNr
  (set-face "divider"             dragon-black0)                 ; #0d0c0c  WinSeparator
  (set-face "error"               samurai-red)                   ; #e82424  diag.error
  (set-face "warning"             ronin-yellow)                  ; #ff9e3b  diag.warning
  (set-face "match"               wave-blue2)                    ; #2d4f67  ui.bg_search
  (set-face "popup"               dragon-black0)                 ; #0d0c0c  ui.float.bg
  (set-face "popup-border"        sumi-ink6)                     ; #54546d  ui.float.fg_border
  (set-face "accent"              dragon-blue2)                  ; #8ba4b0  syn.fun
  )
