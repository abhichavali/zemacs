;;;; rose-pine — all natural pine, faux fur and a bit of soho vibes
;;;;
;;;; Rosé Pine is a palette before it is a theme. `rose-pine/palette' publishes
;;;; fifteen named roles per variant — three grounds (base, surface, overlay),
;;;; three highlights (low, med, high), three foregrounds (muted, subtle, text)
;;;; and six accents (love, gold, rose, pine, foam, iris) — together with a table
;;;; saying what each role is *for*. The reference implementation is
;;;; `rose-pine/neovim'. The values below are the published ones.
;;;;
;;;;   (load-theme "rose-pine")
;;;;
;;;; This is the main variant, the dark one the project leads with; rose-pine-moon
;;;; and rose-pine-dawn sit next to this file and differ only in the palette
;;;; block, a variant here being a second set of hexes for one set of roles.
;;;;
;;;; Every face zemacs has is set below, including the twelve a theme is allowed
;;;; to leave out. Those fall back to a ratio the renderer mixes off the ground,
;;;; and mixing is exactly what Rosé Pine already did by hand: the three
;;;; highlight grounds exist so nobody has to guess, and a port that let the
;;;; renderer guess anyway would throw away a fifth of the palette.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse
;;;; these. A directory is `type', an executable `function', a symlink `link', a
;;;; staged file `string' and an unstaged one `keyword' — and that lands well,
;;;; upstream's Directory being foam and bold.
;;;;
;;;; The font-lock mappings are the neovim implementation's defaults: Keyword
;;;; pine, Function rose, Type foam, String gold, Number gold, Constant gold,
;;;; Comment subtle, Operator subtle, Delimiter and the tree-sitter punctuation
;;;; captures subtle, Identifier text. The palette repository's own role table
;;;; disagrees in one place — it lists pine as the colour for functions and rose
;;;; as the one for booleans — and the implementation wins, because a port is
;;;; judged against the screenshots.
;;;;
;;;; The UI faces are the opposite case: there the table is precise and is
;;;; followed almost to the letter. Highlight low is the cursor line, highlight
;;;; med the selection background paired with a text foreground, muted the
;;;; low-contrast foreground, love and gold the error and warning colours —
;;;; those rows are the spec read straight down. Highlight high is the row that
;;;; is not: the table spends it on the cursor block and on borders, and zemacs
;;;; wants neither of those from it for the reasons below, so it goes to `match'
;;;; instead and the border takes muted, which is what the implementation uses.
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
;;;;   They take muted, one step dimmer, and the value upstream's own retired
;;;;   punctuation option defaulted to.
;;;; * `modeline' and `modeline-inactive' — upstream draws both statuslines on
;;;;   its panel ground (surface) and tells them apart by foreground, subtle
;;;;   against muted. zemacs writes both bars with one `modeline-text', so the
;;;;   difference has to live in the bar: overlay raises the focused one off the
;;;;   buffer, surface leaves the rest on upstream's panel. `modeline-text' is
;;;;   then bold and text rather than the dimmer subtle written on the bar, for
;;;;   the same reason Emacs bolds the informative half of its mode line.
;;;; * `cursor' — the spec pairs a text glyph over a highlight-high block. zemacs
;;;;   knocks the glyph under the caret out to the *background*, so the pairing
;;;;   inverts: the block takes text and the glyph comes out dark, which is the
;;;;   caret every terminal draws and the ratio the renderer defaulted to.
;;;; * `match' — upstream blends gold at twenty percent into the ground for a
;;;;   search hit and paints the *current* hit solid gold. zemacs has one flat
;;;;   band for every hit, so it takes highlight high: one rung past the
;;;;   selection band, so a hit inside a selection still reads as a hit.
;;;;
;;;; Every published colour is spent. leaf #95b1ac is not bound, and is a
;;;; neovim-only seventh accent the palette repository does not carry at all.

(in-package :zemacs)

;;; The palette, named as Rosé Pine names it.
(let (;; Grounds
      (base           '(0.098 0.090 0.141))  ; #191724
      (surface        '(0.122 0.114 0.180))  ; #1f1d2e
      (overlay        '(0.149 0.137 0.227))  ; #26233a

      ;; Highlights — the three UI grounds
      (highlight-low  '(0.129 0.125 0.180))  ; #21202e
      (highlight-med  '(0.251 0.239 0.322))  ; #403d52
      (highlight-high '(0.322 0.310 0.404))  ; #524f67

      ;; Foregrounds
      (muted          '(0.431 0.416 0.525))  ; #6e6a86
      (subtle         '(0.565 0.549 0.667))  ; #908caa
      (text           '(0.878 0.871 0.957))  ; #e0def4

      ;; Accents
      (love           '(0.922 0.435 0.573))  ; #eb6f92
      (gold           '(0.965 0.757 0.467))  ; #f6c177
      (rose           '(0.922 0.737 0.729))  ; #ebbcba
      (pine           '(0.192 0.455 0.561))  ; #31748f
      (foam           '(0.612 0.812 0.847))  ; #9ccfd8
      (iris           '(0.769 0.655 0.906))  ; #c4a7e7
      )

  (apply #'set-background base)
  (apply #'set-foreground text)

  ;; The core 22. Anything skipped keeps the last theme's colour and weight.
  ;; The trailing name is the upstream highlight group the mapping came from.
  (set-face "default"             text)                 ; #e0def4  Normal
  (set-face "keyword"             pine)                 ; #31748f  Keyword
  (set-face "function"            rose)                 ; #ebbcba  Function
  (set-face "type"                foam)                 ; #9ccfd8  Type, Directory
  (set-face "string"              gold)                 ; #f6c177  String
  (set-face "number"              gold)                 ; #f6c177  Number
  (set-face "comment"             subtle   :italic t)   ; #908caa  Comment
  (set-face "constant"            gold)                 ; #f6c177  Constant
  (set-face "variable"            text     :italic t)   ; #e0def4  Identifier, @variable
  (set-face "operator"            subtle)               ; #908caa  Operator
  (set-face "punctuation"         subtle)               ; #908caa  Delimiter
  (set-face "heading-1"           iris     :bold t)     ; #c4a7e7  h1
  (set-face "heading-2"           foam     :bold t)     ; #9ccfd8  h2
  (set-face "heading-3"           rose     :bold t)     ; #ebbcba  h3
  (set-face "bold"                text     :bold t)     ; #e0def4  @markup.strong
  (set-face "italic"              text     :italic t)   ; #e0def4  @markup.italic
  (set-face "link"                iris)                 ; #c4a7e7  the link role
  (set-face "code"                foam)                 ; #9ccfd8  mkdCode, upright
  (set-face "markup"              muted)                ; #6e6a86  see the header
  (set-face "modeline"            overlay)              ; #26233a  bar, current window
  (set-face "modeline-inactive"   surface)              ; #1f1d2e  bar, other windows
  (set-face "modeline-text"       text     :bold t)     ; #e0def4  what is written on it

  ;; The UI 12, which a theme may leave out and this one does not.
  (set-face "region"              highlight-med)        ; #403d52  selection background
  (set-face "cursor"              text)                 ; #e0def4  see the header
  (set-face "current-line"        highlight-low)        ; #21202e  cursor line
  (set-face "line-number"         muted)                ; #6e6a86  LineNr
  (set-face "line-number-current" text)                 ; #e0def4  CursorLineNr
  (set-face "divider"             muted)                ; #6e6a86  WinSeparator
  (set-face "error"               love)                 ; #eb6f92  the error role
  (set-face "warning"             gold)                 ; #f6c177  the warn role
  (set-face "match"               highlight-high)       ; #524f67  see the header
  (set-face "popup"               surface)              ; #1f1d2e  NormalFloat, the panel role
  (set-face "popup-border"        muted)                ; #6e6a86  FloatBorder
  (set-face "accent"              iris)                 ; #c4a7e7  links, hints, staged hunks
  )
