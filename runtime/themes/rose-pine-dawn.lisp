;;;; rose-pine-dawn — the light one, soho vibes with the shutters open
;;;;
;;;; The light variant, and the one that proves Rosé Pine is a palette rather
;;;; than a filter: dawn is not main inverted. `rose-pine/palette' publishes it
;;;; as its own set of fifteen hexes, no two of which are shared — the accents
;;;; are not lightened but *darkened*, because on a #faf4ed ground a pastel is
;;;; nothing at all. The ramps turn over as well, which is the trap in porting
;;;; this variant: surface is #fffaf3, *lighter* than base, while overlay is
;;;; #f2e9e1, darker. A file that copies its sibling's grounds by name gets a
;;;; bar it cannot see.
;;;;
;;;;   (load-theme "rose-pine-dawn")
;;;;
;;;; The assignments are shared with rose-pine and rose-pine-moon, which sit next
;;;; to this file, and come from the reference implementation `rose-pine/neovim'.
;;;; The values are the published ones, with one caveat worth stating: dawn's
;;;; body colour is #464261 here, which is what both `rose-pine/palette' and
;;;; `rose-pine/neovim' carry today. A great many ports in the wild — and most
;;;; screenshots — still show the older #575279 that dawn shipped with for years.
;;;; The newer one is darker, 8.7:1 on the ground rather than 6.7:1, and upstream
;;;; is followed here.
;;;;
;;;; Every face zemacs has is set below, including the twelve a theme is allowed
;;;; to leave out. On a light theme that omission is worse than usual, since the
;;;; face surviving from the theme before is overwhelmingly one drawn to be
;;;; legible on a dark ground.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse
;;;; these. A directory is `type', an executable `function', a symlink `link', a
;;;; staged file `string' and an unstaged one `keyword' — and that lands well,
;;;; upstream's Directory being foam and bold.
;;;;
;;;; The font-lock mappings are the neovim implementation's defaults: Keyword
;;;; pine, Function rose, Type foam, String gold, Number gold, Constant gold,
;;;; Comment subtle, Operator subtle, Delimiter and the punctuation captures
;;;; subtle, Identifier text. The UI faces come from the palette repository's own
;;;; role table, which is precise about them: highlight low is the cursor line,
;;;; highlight med the selection background paired with a text foreground, muted
;;;; the low-contrast foreground, love and gold the error and warning colours.
;;;; Highlight high is the one row not taken at its word — the table spends it on
;;;; the cursor block and on borders, and both of those go elsewhere here for the
;;;; reasons below, so it goes to `match' and the border takes muted, which is
;;;; what the implementation uses.
;;;;
;;;; A word about contrast, since the Modus files sitting next to this one make
;;;; a point of it: dawn does not have that property and this port does not
;;;; pretend otherwise. Body text on base is 8.7:1 and subtle is 4.0:1, but the
;;;; warm accents fall away — gold is 2.1:1 and rose 2.6:1, so literals and
;;;; function names are the weakest things on the screen. A dawn repainted until
;;;; every hue passed WCAG would not be dawn. What a port can do without lying
;;;; about the palette is refuse to let structure depend on hue alone, which is
;;;; what the weight below is for.
;;;;
;;;; On weight and slant: five faces are bold and three are italic. Rosé Pine
;;;; ships `styles.italic' on by default and applies it to comments *and to
;;;; plain identifiers* — its `@variable' capture is text plus italic — so a
;;;; source buffer in this theme has slanted identifiers in it. That is the most
;;;; visible thing about the theme in the wild, and it is kept. Two emphases
;;;; upstream does apply are dropped: its mkdCode is foam *and italic* where
;;;; inline code is upright here, a monospace span being the one place the
;;;; effect costs legibility rather than paying for it, and its CursorLineNr is
;;;; bold where `line-number-current' is plain, a gutter digit having nothing to
;;;; be emphatic about.
;;;;
;;;; Six mappings are not Rosé Pine's own:
;;;;
;;;; * `keyword' — upstream has two answers. Its Keyword group is plain pine and
;;;;   its Statement group is pine and bold. zemacs paints both with one face,
;;;;   and bolding every `if' in the buffer is a louder theme than this one.
;;;; * `heading-1' through `heading-3' — upstream grades six levels (iris, foam,
;;;;   rose, gold, pine, leaf) and zemacs has three, which take the first three
;;;;   unchanged. The deviation is at the bottom: level 3 here is also levels 4,
;;;;   5 and 6, so gold, pine and leaf never get their turn.
;;;; * `markup' — the delimiters themselves, the asterisks around an emphasised
;;;;   word. Upstream's markdownDelimiter is subtle, which is exactly the comment
;;;;   colour and would leave the delimiters as loud as the word between them.
;;;;   They take muted, which on this variant is the *lighter* of the two greys
;;;;   and therefore still the dimmer one, the ramp having turned over.
;;;; * `modeline' and `modeline-inactive' — the deviation dawn forces. Upstream
;;;;   draws both statuslines on its panel ground and tells them apart by
;;;;   foreground; zemacs writes both bars with one `modeline-text', so the
;;;;   difference has to live in the bar, and here the panel ground is brighter
;;;;   than the buffer it sits against. The focused bar sinks to overlay, the
;;;;   rest stay on upstream's panel and all but vanish into the page, which for
;;;;   an inactive window is the right amount of presence. `modeline-text' is
;;;;   bold and text rather than the dimmer subtle written on the bar, for the
;;;;   same reason Emacs bolds the informative half of its mode line.
;;;; * `cursor' — the spec pairs a text glyph over a highlight-high block. zemacs
;;;;   knocks the glyph under the caret out to the *background*, so the pairing
;;;;   inverts: the block takes text and the glyph comes out pale, which is the
;;;;   caret every terminal draws and the ratio the renderer defaulted to.
;;;; * `match' — upstream blends gold at twenty percent into the ground for a
;;;;   search hit and paints the *current* hit solid gold. zemacs has one flat
;;;;   band for every hit, so it takes highlight high: one rung past the
;;;;   selection band, so a hit inside a selection still reads as a hit.
;;;;
;;;; Every published colour is spent. leaf #6d8f89 is not bound, and is a
;;;; neovim-only seventh accent the palette repository does not carry at all.

(in-package :zemacs)

;;; The palette, named as Rosé Pine names it. Note that the ramp runs the other
;;; way here: surface is brighter than base and overlay is darker.
(let (;; Grounds
      (base           '(0.980 0.957 0.929))  ; #faf4ed
      (surface        '(1.000 0.980 0.953))  ; #fffaf3
      (overlay        '(0.949 0.914 0.882))  ; #f2e9e1

      ;; Highlights — the three UI grounds
      (highlight-low  '(0.957 0.929 0.910))  ; #f4ede8
      (highlight-med  '(0.875 0.855 0.851))  ; #dfdad9
      (highlight-high '(0.808 0.792 0.804))  ; #cecacd

      ;; Foregrounds
      (muted          '(0.596 0.576 0.647))  ; #9893a5
      (subtle         '(0.475 0.459 0.576))  ; #797593
      (text           '(0.275 0.259 0.380))  ; #464261

      ;; Accents
      (love           '(0.706 0.388 0.478))  ; #b4637a
      (gold           '(0.918 0.616 0.204))  ; #ea9d34
      (rose           '(0.843 0.510 0.494))  ; #d7827e
      (pine           '(0.157 0.412 0.514))  ; #286983
      (foam           '(0.337 0.580 0.624))  ; #56949f
      (iris           '(0.565 0.478 0.663))  ; #907aa9
      )

  (apply #'set-background base)
  (apply #'set-foreground text)

  ;; The core 22. Anything skipped keeps the last theme's colour and weight.
  ;; The trailing name is the upstream highlight group the mapping came from.
  (set-face "default"             text)                 ; #464261  Normal
  (set-face "keyword"             pine)                 ; #286983  Keyword
  (set-face "function"            rose)                 ; #d7827e  Function
  (set-face "type"                foam)                 ; #56949f  Type, Directory
  (set-face "string"              gold)                 ; #ea9d34  String
  (set-face "number"              gold)                 ; #ea9d34  Number
  (set-face "comment"             subtle   :italic t)   ; #797593  Comment
  (set-face "constant"            gold)                 ; #ea9d34  Constant
  (set-face "variable"            text     :italic t)   ; #464261  Identifier, @variable
  (set-face "operator"            subtle)               ; #797593  Operator
  (set-face "punctuation"         subtle)               ; #797593  Delimiter
  (set-face "heading-1"           iris     :bold t)     ; #907aa9  h1
  (set-face "heading-2"           foam     :bold t)     ; #56949f  h2
  (set-face "heading-3"           rose     :bold t)     ; #d7827e  h3
  (set-face "bold"                text     :bold t)     ; #464261  @markup.strong
  (set-face "italic"              text     :italic t)   ; #464261  @markup.italic
  (set-face "link"                iris)                 ; #907aa9  the link role
  (set-face "code"                foam)                 ; #56949f  mkdCode, upright
  (set-face "markup"              muted)                ; #9893a5  see the header
  (set-face "modeline"            overlay)              ; #f2e9e1  bar, current window
  (set-face "modeline-inactive"   surface)              ; #fffaf3  bar, other windows
  (set-face "modeline-text"       text     :bold t)     ; #464261  what is written on it

  ;; The UI 12, which a theme may leave out and this one does not.
  (set-face "region"              highlight-med)        ; #dfdad9  selection background
  (set-face "cursor"              text)                 ; #464261  see the header
  (set-face "current-line"        highlight-low)        ; #f4ede8  cursor line
  (set-face "line-number"         muted)                ; #9893a5  LineNr
  (set-face "line-number-current" text)                 ; #464261  CursorLineNr
  (set-face "divider"             muted)                ; #9893a5  WinSeparator
  (set-face "error"               love)                 ; #b4637a  the error role
  (set-face "warning"             gold)                 ; #ea9d34  the warn role
  (set-face "match"               highlight-high)       ; #cecacd  see the header
  (set-face "popup"               surface)              ; #fffaf3  NormalFloat, the panel role
  (set-face "popup-border"        muted)                ; #9893a5  FloatBorder
  (set-face "accent"              iris)                 ; #907aa9  links, hints, staged hunks
  )
