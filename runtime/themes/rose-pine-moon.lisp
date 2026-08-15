;;;; rose-pine-moon — the same soho vibes, one shade further from the void
;;;;
;;;; The middle variant. Rosé Pine's moon is not the main variant lightened by a
;;;; slider: `rose-pine/palette' publishes it as its own set of fifteen hexes, of
;;;; which eight differ. The ground rises from #191724 to #232136 and takes
;;;; surface, overlay and all three highlights up with it, and two accents are
;;;; redrawn — rose warms from #ebbcba to #ea9a97, pine brightens from #31748f
;;;; to #3e8fb0. The second is the one you notice, because pine is what every
;;;; keyword is painted with and main's is dark enough on its own ground to read
;;;; as reserved.
;;;;
;;;;   (load-theme "rose-pine-moon")
;;;;
;;;; Everything else is rose-pine, and deliberately so: the palette block below
;;;; is the only thing that differs from that file. The values are the published
;;;; ones from `rose-pine/palette'; the highlight groups they are spent on are
;;;; the reference implementation's — `rose-pine/neovim'.
;;;;
;;;; Every face zemacs has is set below, including the twelve a theme is allowed
;;;; to leave out. That matters most between siblings: switching main to moon
;;;; touches eight values, and a face forgotten here is a face left at the wrong
;;;; eight — colour and, since faces carry weight now, emphasis with it.
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
;;;; Moon's highlights sit shallower against its ground than main's do against
;;;; theirs, at every rung of the ramp, which is part of why it reads softer.
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
;;;;   punctuation option defaulted to — a value moon shares with main unchanged,
;;;;   so against this variant's higher ground it is quieter still.
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

;;; The palette, named as Rosé Pine names it. Eight values differ from main:
;;; the three grounds, the three highlights and two of the six accents.
(let (;; Grounds
      (base           '(0.137 0.129 0.212))  ; #232136
      (surface        '(0.165 0.153 0.247))  ; #2a273f
      (overlay        '(0.224 0.208 0.322))  ; #393552

      ;; Highlights — the three UI grounds
      (highlight-low  '(0.165 0.157 0.243))  ; #2a283e
      (highlight-med  '(0.267 0.255 0.353))  ; #44415a
      (highlight-high '(0.337 0.322 0.431))  ; #56526e

      ;; Foregrounds — shared with main, all three of them
      (muted          '(0.431 0.416 0.525))  ; #6e6a86
      (subtle         '(0.565 0.549 0.667))  ; #908caa
      (text           '(0.878 0.871 0.957))  ; #e0def4

      ;; Accents
      (love           '(0.922 0.435 0.573))  ; #eb6f92
      (gold           '(0.965 0.757 0.467))  ; #f6c177
      (rose           '(0.918 0.604 0.592))  ; #ea9a97
      (pine           '(0.243 0.561 0.690))  ; #3e8fb0
      (foam           '(0.612 0.812 0.847))  ; #9ccfd8
      (iris           '(0.769 0.655 0.906))  ; #c4a7e7
      )

  (apply #'set-background base)
  (apply #'set-foreground text)

  ;; The core 22. Anything skipped keeps the last theme's colour and weight.
  ;; The trailing name is the upstream highlight group the mapping came from.
  (set-face "default"             text)                 ; #e0def4  Normal
  (set-face "keyword"             pine)                 ; #3e8fb0  Keyword
  (set-face "function"            rose)                 ; #ea9a97  Function
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
  (set-face "heading-3"           rose     :bold t)     ; #ea9a97  h3
  (set-face "bold"                text     :bold t)     ; #e0def4  @markup.strong
  (set-face "italic"              text     :italic t)   ; #e0def4  @markup.italic
  (set-face "link"                iris)                 ; #c4a7e7  the link role
  (set-face "code"                foam)                 ; #9ccfd8  mkdCode, upright
  (set-face "markup"              muted)                ; #6e6a86  see the header
  (set-face "modeline"            overlay)              ; #393552  bar, current window
  (set-face "modeline-inactive"   surface)              ; #2a273f  bar, other windows
  (set-face "modeline-text"       text     :bold t)     ; #e0def4  what is written on it

  ;; The UI 12, which a theme may leave out and this one does not.
  (set-face "region"              highlight-med)        ; #44415a  selection background
  (set-face "cursor"              text)                 ; #e0def4  see the header
  (set-face "current-line"        highlight-low)        ; #2a283e  cursor line
  (set-face "line-number"         muted)                ; #6e6a86  LineNr
  (set-face "line-number-current" text)                 ; #e0def4  CursorLineNr
  (set-face "divider"             muted)                ; #6e6a86  WinSeparator
  (set-face "error"               love)                 ; #eb6f92  the error role
  (set-face "warning"             gold)                 ; #f6c177  the warn role
  (set-face "match"               highlight-high)       ; #56526e  see the header
  (set-face "popup"               surface)              ; #2a273f  NormalFloat, the panel role
  (set-face "popup-border"        muted)                ; #6e6a86  FloatBorder
  (set-face "accent"              iris)                 ; #c4a7e7  links, hints, staged hunks
  )
