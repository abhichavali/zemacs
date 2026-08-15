;;;; modus-vivendi-tinted — Modus Vivendi with the black taken out of it
;;;;
;;;; Same theme, same contrast discipline, warmer ground: the background is a
;;;; deep desaturated indigo (#0d0e1c) rather than pure black, and the modeline
;;;; and accents are tinted to sit on it. Protesilaos ships this for the same
;;;; reason people reach for it — #000000 on a modern panel is a hole in the
;;;; screen, and a near-black with a hue in it reads as a surface.
;;;;
;;;; Every colour is the published `modus-themes-vivendi-tinted-palette' value,
;;;; not an approximation, so the AAA (7:1) contrast the Modus themes exist to
;;;; guarantee survives the port.
;;;;
;;;; Load it from your init.lisp — these are ordinary top-level forms, so
;;;; loading the file *is* applying the theme:
;;;;
;;;;   (load-theme "modus-vivendi-tinted")
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. A face left out here is a face the previous theme still
;;;; owns, which is the whole reason the list is exhaustive and boring.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse
;;;; these. A directory is `type', an executable `function', a symlink `link',
;;;; a staged file `string' and an unstaged one `keyword'. So the same lines
;;;; that colour a source buffer colour those too.
;;;;
;;;; The mappings that are not Modus' own are the same ones the plain vivendi
;;;; file documents, and for the same reasons: `number' borrows `identifier',
;;;; the three `heading-N' take Modus' coloured levels 0/2/3 *and* the bold
;;;; weight Modus puts on its headings, and `bold'/`italic' take
;;;; `fg-prose-verbatim'/`docstring' on top of the real weight and slant they
;;;; now carry.
;;;;
;;;; Nothing else in this file is bold. Modus' own `modus-themes-bold-constructs'
;;;; and `modus-themes-italic-constructs' both default to nil — keywords are not
;;;; heavy and comments are not slanted in Modus, and a port that reached for the
;;;; new weight everywhere would stop being this theme.
;;;;
;;;; What you actually see may still be heavier, because `*bold-constructs*' in
;;;; init.lisp is applied on top of whichever theme is loaded and bolds
;;;; keywords, functions and types by default. That is zemacs' taste rather than
;;;; Modus', and it is a variable for exactly that reason — set it to NIL for
;;;; this theme as Protesilaos published it.
;;;;
;;;; The twelve optional UI faces read straight down the same palette:
;;;; `bg-region', `bg-hl-line' and `bg-popup' from its special-purpose block,
;;;; `border' for the pane rule and the popup stroke both, and `cursor', `err'
;;;; and `warning' from its mappings — the caret magenta here where the plain
;;;; theme's is white, Modus' own choice rather than a liberty taken. Three are
;;;; this port's: one `popup' against Modus' separate company, corfu and
;;;; posframe grounds, one `accent' against its graded `accent-0' to `accent-3'
;;;; — both take the first — and `match' on `bg-search-current' alone.

(in-package :zemacs)

;;; The palette, named as Modus names it.
(let (;; Basic values
      (bg-main               '(0.051 0.055 0.110))  ; #0d0e1c
      (fg-main               '(1.000 1.000 1.000))  ; #ffffff
      ;; `#9d9d9d' is a grey, so all three components are equal. The blue used
      ;; to read 0.667, which is 0xaa/255 — the *alpha* byte of the source value
      ;; `#9d9d9daa', copied into the channel below it. A grey with a blue cast,
      ;; on the face this theme dims half its furniture with.
      (fg-dim                '(0.616 0.616 0.616))  ; #9d9d9d (from #9d9d9daa)
      (border                '(0.380 0.392 0.478))  ; #61647a

      ;; Accent foregrounds. Identical to plain vivendi: the accents are chosen
      ;; against `fg-main', and the ground moving does not move them.
      (blue-warmer           '(0.475 0.659 1.000))  ; #79a8ff
      (blue-cooler           '(0.000 0.737 1.000))  ; #00bcff
      (blue-faint            '(0.510 0.690 0.925))  ; #82b0ec
      (cyan                  '(0.000 0.827 0.816))  ; #00d3d0
      (cyan-cooler           '(0.416 0.894 0.725))  ; #6ae4b9
      (green-faint           '(0.533 0.792 0.624))  ; #88ca9f
      (magenta               '(0.996 0.675 0.816))  ; #feacd0
      (magenta-warmer        '(0.969 0.561 0.906))  ; #f78fe7
      (magenta-cooler        '(0.714 0.627 1.000))  ; #b6a0ff
      (magenta-intense       '(1.000 0.400 1.000))  ; #ff66ff
      (red                   '(1.000 0.373 0.349))  ; #ff5f59
      (yellow-warmer         '(0.996 0.769 0.247))  ; #fec43f
      (yellow-faint          '(0.824 0.710 0.502))  ; #d2b580

      ;; Accent backgrounds
      (bg-yellow-intense     '(0.478 0.380 0.000))  ; #7a6100

      ;; Special purpose
      (bg-popup              '(0.078 0.086 0.173))  ; #14162c
      (bg-hl-line            '(0.188 0.227 0.435))  ; #303a6f
      (bg-region             '(0.333 0.353 0.400))  ; #555a66

      ;; Modeline backgrounds — tinted, which is the whole point of the variant.
      (bg-mode-line-active   '(0.267 0.247 0.376))  ; #443f60
      (bg-mode-line-inactive '(0.157 0.157 0.208))  ; #282835
      )

  (apply #'set-background bg-main)
  (apply #'set-foreground fg-main)

  ;; All of them. Anything skipped keeps the last theme's colour *and weight*,
  ;; which is the half that bites: an unset face inheriting a bold from the
  ;; theme before it looks like a highlighter bug rather than a missing line.
  (set-face "default"           fg-main)                ; #ffffff  Modus `fg-main'
  (set-face "keyword"           magenta-cooler)         ; #b6a0ff  Modus `keyword'
  (set-face "function"          magenta)                ; #feacd0  Modus `fnname'
  (set-face "type"              cyan-cooler)            ; #6ae4b9  Modus `type'
  (set-face "string"            blue-warmer)            ; #79a8ff  Modus `string'
  (set-face "number"            yellow-faint)           ; #d2b580  Modus `identifier'
  (set-face "comment"           fg-dim)                 ; #9d9d9d  Modus `comment'
  (set-face "constant"          blue-cooler)            ; #00bcff  Modus `constant'
  (set-face "variable"          cyan)                   ; #00d3d0  Modus `variable'
  (set-face "operator"          fg-main)                ; #ffffff  Modus `operator'
  (set-face "punctuation"       fg-main)                ; #ffffff  Modus `punctuation'
  (set-face "heading-1"         cyan-cooler   :bold t)  ; #6ae4b9  Modus `fg-heading-0'
  (set-face "heading-2"         yellow-faint  :bold t)  ; #d2b580  Modus `fg-heading-2'
  (set-face "heading-3"         blue-faint    :bold t)  ; #82b0ec  Modus `fg-heading-3'
  (set-face "bold"              magenta-warmer :bold t) ; #f78fe7  Modus `fg-prose-verbatim'
  (set-face "italic"            green-faint :italic t)  ; #88ca9f  Modus `docstring'
  (set-face "link"              blue-warmer)            ; #79a8ff  Modus `fg-link'
  (set-face "code"              cyan-cooler)            ; #6ae4b9  Modus `fg-prose-code'
  (set-face "markup"            fg-dim)                 ; #9d9d9d  Modus `prose-metadata'
  (set-face "modeline"          bg-mode-line-active)    ; #443f60  Modus `bg-mode-line-active'
  (set-face "modeline-inactive" bg-mode-line-inactive)  ; #282835  Modus `bg-mode-line-inactive'
  (set-face "modeline-text"     fg-main)                ; #ffffff  Modus `fg-mode-line-active'

  ;; The twelve optional ones. Left unset they fall back to a ratio the renderer
  ;; mixes off the ground — a guess, in the one theme that publishes a measured
  ;; value for every band it draws.
  (set-face "region"              bg-region)          ; #555a66  Modus `bg-region'
  (set-face "cursor"              magenta-intense)    ; #ff66ff  Modus `cursor'
  (set-face "current-line"        bg-hl-line)         ; #303a6f  Modus `bg-hl-line'
  (set-face "line-number"         fg-dim)             ; #9d9d9d  Modus `fg-line-number-inactive'
  (set-face "line-number-current" fg-main)            ; #ffffff  Modus `fg-line-number-active'
  (set-face "divider"             border)             ; #61647a  Modus `border'
  (set-face "error"               red)                ; #ff5f59  Modus `err'
  (set-face "warning"             yellow-warmer)      ; #fec43f  Modus `warning'
  (set-face "match"               bg-yellow-intense)  ; #7a6100  Modus `bg-search-current'
  (set-face "popup"               bg-popup)           ; #14162c  Modus `bg-popup'
  (set-face "popup-border"        border)             ; #61647a  Modus `child-frame-border'
  (set-face "accent"              blue-cooler)        ; #00bcff  Modus `accent-0'
  )
