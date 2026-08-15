;;;; melange-dark — paper and ink under a lamp, and the softest theme here
;;;;
;;;; Melange is Sergio A. Vargas' `savq/melange-nvim'. The palette below is
;;;; `lua/melange/palettes/dark.lua' read off the source rather than off a
;;;; screenshot, and every assignment is `colors/melange.lua' in the same
;;;; repository. Upstream sorts its colours into four lettered groups and the
;;;; names below keep the letters: a is the greys, ground up to body text; b
;;;; the bright foreground colours; c the same six one step duller; d a set of
;;;; *background* colours, dark enough to sit behind text. Upstream's key for
;;;; a.float is spelt overbg here, because a lexical binding named float is a
;;;; symbol in the COMMON-LISP package and binding one is undefined behaviour.
;;;; There is no drop shadow colour in Melange — second-hand listings of the
;;;; palette sometimes carry one; the upstream file has six greys and that is
;;;; all, so nothing below is darker than a.bg.
;;;;
;;;;   (load-theme "melange-dark")
;;;;
;;;; Every face is set below, so nothing survives from whatever was loaded
;;;; before: a face left out is one the previous theme still owns, its colour
;;;; *and its weight*, which is the worse half. Hence a list that is exhaustive
;;;; and boring. Dired and the git status buffer have no faces of their own
;;;; either — a directory is `type', an executable `function', a symlink
;;;; `link', a staged file `string', an unstaged one `keyword'.
;;;;
;;;; Say it plainly: this is the softest theme in the directory and nothing in
;;;; it is loud. Every hue sits in a narrow warm band — the greys are browns,
;;;; even the blue is a dusty lavender — and the accents are muted twice over,
;;;; once by group b and again by group c. Where Dracula puts a #ff79c6 on a
;;;; near-black, Melange puts a #e49b5d on a #292522. Nothing here has been
;;;; brightened to read better in a screenshot; it is the theme to load when a
;;;; screen must be looked at all day rather than admired for a minute.
;;;;
;;;; Upstream is more italic than most and barely bold at all. Comments are
;;;; italic, which is ordinary, and *strings are italic too*, which is not —
;;;; that is `colors/melange.lua' doing it, not this port. Bold goes only to
;;;; headings and to a matched span in a completion list, so keyword, type and
;;;; function are carried by hue alone rather than invented into boldness. The
;;;; heading ladder is upstream's own and is no ramp: orange, gold, sage.
;;;;
;;;; Six mappings are not Melange's own:
;;;;
;;;; * `type' — upstream paints Type c.cyan and a directory c.green. zemacs
;;;;   folds dired's directories in, and the face takes c.cyan because a source
;;;;   buffer is where it is mostly seen; c.green goes unspent and is unbound.
;;;; * `punctuation' — split upstream too: brackets take its Delimiter group,
;;;;   d.yellow, a background colour pressed into service as a foreground and
;;;;   the dimmest thing here that is still text; commas take c.red. The one
;;;;   face takes d.yellow, because brackets outnumber commas and because a
;;;;   screen of dusty-rose commas is louder than Melange ever gets. `markup'
;;;;   takes d.yellow as well, which *is* upstream — a bullet is a Delimiter.
;;;; * `link' — upstream's markup link is an underline with no colour at all,
;;;;   and zemacs does not underline. It takes c.blue, upstream's URL colour.
;;;; * `modeline' and `modeline-inactive' — upstream draws the status line and
;;;;   its inactive twin on one a.float and tells them apart by foreground.
;;;;   zemacs writes both bars with one `modeline-text', so the difference has
;;;;   to live in the bar: the focused one takes a.sel, one surface further off
;;;;   the ground, and the rest keep a.float. That `modeline-text' is bold is
;;;;   the other half of the same compromise — with one face for the whole bar
;;;;   the choice is all or nothing, and bold is what stops the bar reading as
;;;;   one more line of the buffer.
;;;; * `cursor' and `popup-border' — the Cursor and float-border groups are
;;;;   both commented out upstream, left to the terminal or to defaults. The
;;;;   caret takes a.fg, which is what every terminal config Melange generates
;;;;   from `lua/melange/build.lua' sets it to; the stroke takes a.sel, the
;;;;   next surface up from the a.float it encloses.
;;;; * `accent' — b.yellow, upstream's colour for the matched span of a
;;;;   completion candidate and for the insert-mode indicator on its bar. It
;;;;   equals `function', which is the fallback when unset; naming it says the
;;;;   equality is meant rather than lucky.
;;;;
;;;; `variable' is a.fg, upstream's Identifier, and worth saying out loud:
;;;; Melange paints only the identifiers that are something in particular and
;;;; leaves the rest in the body colour. It is quiet partly because so much of
;;;; a buffer is simply left alone.

(in-package :zemacs)

;;; The palette, grouped as `dark.lua' groups it.
(let (;; Group a — the greys, ground up to body text
      (bg        '(0.161 0.145 0.133))  ; #292522
      (overbg    '(0.204 0.188 0.173))  ; #34302c
      (sel       '(0.251 0.227 0.212))  ; #403a36
      (ui        '(0.525 0.455 0.384))  ; #867462
      (com       '(0.757 0.655 0.557))  ; #c1a78e
      (fg        '(0.925 0.882 0.843))  ; #ece1d7

      ;; Group b — the bright foreground colours
      (b-red     '(0.831 0.467 0.400))  ; #d47766
      (b-yellow  '(0.922 0.753 0.427))  ; #ebc06d
      (b-green   '(0.522 0.714 0.584))  ; #85b695
      (b-cyan    '(0.537 0.702 0.714))  ; #89b3b6
      (b-blue    '(0.639 0.663 0.808))  ; #a3a9ce
      (b-magenta '(0.812 0.608 0.761))  ; #cf9bc2

      ;; Group c — the same six, one step duller
      (c-red     '(0.741 0.506 0.514))  ; #bd8183
      (c-yellow  '(0.894 0.608 0.365))  ; #e49b5d
      (c-cyan    '(0.482 0.588 0.584))  ; #7b9695
      (c-blue    '(0.498 0.569 0.698))  ; #7f91b2
      (c-magenta '(0.702 0.502 0.690))  ; #b380b0

      ;; Group d — the background colours. One is spent, and in both roles:
      ;; upstream's Delimiter group, where it is a foreground and the dimmest
      ;; text here, and the band under a search hit, where it is a background.
      (d-yellow  '(0.545 0.455 0.286))  ; #8b7449
      )

  (apply #'set-background bg)
  (apply #'set-foreground fg)

  ;; The 22 core faces. Anything skipped keeps the last theme's colour and
  ;; weight. The trailing name is the group in `colors/melange.lua' the
  ;; mapping came from.
  (set-face "default"            fg)                    ; #ece1d7  Normal
  (set-face "keyword"            c-yellow)              ; #e49b5d  Statement
  (set-face "function"           b-yellow)              ; #ebc06d  Function
  (set-face "type"               c-cyan)                ; #7b9695  Type
  (set-face "string"             b-blue    :italic t)   ; #a3a9ce  String
  (set-face "number"             b-magenta)             ; #cf9bc2  Number
  (set-face "comment"            com       :italic t)   ; #c1a78e  Comment
  (set-face "constant"           c-magenta)             ; #b380b0  Constant
  (set-face "variable"           fg)                    ; #ece1d7  Identifier
  (set-face "operator"           b-red)                 ; #d47766  Operator
  (set-face "punctuation"        d-yellow)              ; #8b7449  Delimiter
  (set-face "heading-1"          c-yellow  :bold t)     ; #e49b5d  Title
  (set-face "heading-2"          b-yellow  :bold t)     ; #ebc06d  @markup.heading.2
  (set-face "heading-3"          b-green   :bold t)     ; #85b695  @markup.heading.3
  (set-face "bold"               fg        :bold t)     ; #ece1d7  Bold, weight only
  (set-face "italic"             fg        :italic t)   ; #ece1d7  Italic, slant only
  (set-face "link"               c-blue)                ; #7f91b2  see the header
  (set-face "code"               b-cyan)                ; #89b3b6  @markup.raw
  (set-face "markup"             d-yellow)              ; #8b7449  @markup.list
  (set-face "modeline"           sel)                   ; #403a36  bar, current window
  (set-face "modeline-inactive"  overbg)                ; #34302c  bar, other windows
  (set-face "modeline-text"      fg        :bold t)     ; #ece1d7  text on the bar

  ;; The 12 UI faces. Optional in general — each falls back to a mix of the
  ;; ground and the body colour — but a theme this quiet cannot afford a
  ;; computed selection band, so all of them are named.
  (set-face "region"             sel)                   ; #403a36  Visual
  (set-face "cursor"             fg)                    ; #ece1d7  see the header
  (set-face "current-line"       overbg)                ; #34302c  CursorLine
  (set-face "line-number"        ui)                    ; #867462  LineNr
  (set-face "line-number-current" c-yellow)             ; #e49b5d  CursorLineNr
  (set-face "divider"            ui)                    ; #867462  WinSeparator
  (set-face "error"              c-red)                 ; #bd8183  DiagnosticError
  (set-face "warning"            b-yellow)              ; #ebc06d  DiagnosticWarn
  (set-face "match"              d-yellow)              ; #8b7449  Search, background
  (set-face "popup"              overbg)                ; #34302c  NormalFloat, Pmenu
  (set-face "popup-border"       sel)                   ; #403a36  see the header
  (set-face "accent"             b-yellow)              ; #ebc06d  PmenuMatch
  )
