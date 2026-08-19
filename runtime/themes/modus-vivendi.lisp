;;;; modus-vivendi — elegant, highly legible theme with a black background
;;;;
;;;; Protesilaos Stavrou's Modus themes, ported to zemacs. Their point is
;;;; contrast: every colour below is at least 7:1 against the background it is
;;;; drawn on, which is the WCAG AAA threshold. The values are the published
;;;; ones from `modus-themes-vivendi-palette', not approximations of them, so
;;;; that property survives the port.
;;;;
;;;; Load it from your init.lisp — these are ordinary top-level forms, so
;;;; loading the file *is* applying the theme:
;;;;
;;;;   (load (merge-pathnames "themes/modus-vivendi.lisp" *load-truename*))
;;;;
;;;; or an absolute path, if your init.lisp does not sit next to this file.
;;;; Loading the other one afterwards switches themes: every face zemacs has is
;;;; set below, so nothing survives from whatever was loaded before. A face left
;;;; out here is a face the previous theme still owns, which is the whole reason
;;;; the list is exhaustive and boring.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse
;;;; these. A directory is `type', an executable `function', a symlink `link',
;;;; a staged file `string' and an unstaged one `keyword'. So the same 22 lines
;;;; that colour a source buffer colour those too.
;;;;
;;;; Three mappings are not Modus' own:
;;;;
;;;; * `number' — Modus leaves numeric literals at `fg-main'. zemacs also spends
;;;;   this face on dired's size column and magit's counts, where undifferen-
;;;;   tiated foreground is the exact complaint a theme is here to answer, so it
;;;;   borrows Modus' `identifier' colour.
;;;; * `bold' and `italic' — see below. `heading-N' used to be a deviation too:
;;;;   the three took Modus' *coloured* levels 0, 2 and 3, on the argument that
;;;;   zemacs had one heading size and hue was the only thing left to rank three
;;;;   levels with. That argument was never true here — `*org-modern-heading-scale*'
;;;;   has set level 1 at 1.5x and level 2 at 1.25x since before this file was
;;;;   written — and it cost the theme twice over. Level 1 wore `fg-heading-0',
;;;;   which is the colour of a `#+title' and not of a heading, and it is also
;;;;   exactly this theme's `type', so a heading in a document with code in it
;;;;   read as a type name. The levels are Modus' own 1, 2 and 3 now: `fg-main'
;;;;   bold at 1.5x says top-level far more plainly than a hue can, which is
;;;;   why Modus spells it that way.
;;;; * `bold' and `italic' — these used to be colours standing in for weight and
;;;;   slant, the renderer having neither. It has both, so they carry both, and
;;;;   they keep the colours Modus gives the nearest thing it does colour,
;;;;   `fg-prose-verbatim' and `docstring'.
;;;;
;;;; Nothing else in this file is bold, and that is faithful rather than lazy:
;;;; `modus-themes-bold-constructs' and `modus-themes-italic-constructs' both
;;;; default to nil upstream. Modus carries meaning in hue at AAA contrast and
;;;; deliberately does not shout.
;;;;
;;;; What you actually see on the screen may still be heavier than that, because
;;;; `*bold-constructs*' in init.lisp is applied on top of whichever theme is
;;;; loaded and bolds keywords, functions and types by default. That is zemacs'
;;;; taste, not Modus', and it is a variable for exactly that reason — set it to
;;;; NIL for this theme as Protesilaos published it.
;;;;
;;;; The twelve optional UI faces read straight down the same palette: `bg-region',
;;;; `bg-hl-line' and `bg-popup' from its special-purpose block, `border' for both
;;;; the pane rule and the popup stroke, and `cursor', `err' and `warning' from its
;;;; mappings. `bg-hl-line' is a navy, not a few percent off black — louder than the
;;;; usual stripe, and published. Two are this port's: zemacs has one `popup' where
;;;; Modus dresses company, corfu and posframe apart, and one `accent' where Modus
;;;; grades `accent-0' through `accent-3', so both take the first of their series;
;;;; and `match' is `bg-search-current' alone, the lazy-hit cyan having no band.

(in-package :zemacs)

;;; The palette, named as Modus names it.
(let (;; Basic values
      (bg-main               '(0.000 0.000 0.000))  ; #000000
      (fg-main               '(1.000 1.000 1.000))  ; #ffffff
      (fg-dim                '(0.596 0.596 0.596))  ; #989898
      (border                '(0.392 0.392 0.392))  ; #646464

      ;; Accent foregrounds
      (blue-warmer           '(0.475 0.659 1.000))  ; #79a8ff
      (blue-cooler           '(0.000 0.737 1.000))  ; #00bcff
      (blue-faint            '(0.510 0.690 0.925))  ; #82b0ec
      (cyan                  '(0.000 0.827 0.816))  ; #00d3d0
      (cyan-cooler           '(0.416 0.894 0.725))  ; #6ae4b9
      (green-faint           '(0.533 0.792 0.624))  ; #88ca9f
      (magenta               '(0.996 0.675 0.816))  ; #feacd0
      (magenta-warmer        '(0.969 0.561 0.906))  ; #f78fe7
      (magenta-cooler        '(0.714 0.627 1.000))  ; #b6a0ff
      (red                   '(1.000 0.373 0.349))  ; #ff5f59
      (yellow-warmer         '(0.996 0.769 0.247))  ; #fec43f
      (yellow-faint          '(0.824 0.710 0.502))  ; #d2b580

      ;; Accent backgrounds
      (bg-yellow-intense     '(0.478 0.380 0.000))  ; #7a6100

      ;; Special purpose
      (bg-popup              '(0.047 0.047 0.047))  ; #0c0c0c
      (bg-hl-line            '(0.184 0.220 0.286))  ; #2f3849
      (bg-region             '(0.353 0.353 0.353))  ; #5a5a5a

      ;; Modeline backgrounds
      (bg-mode-line-active   '(0.314 0.314 0.314))  ; #505050
      (bg-mode-line-inactive '(0.176 0.176 0.176))  ; #2d2d2d
      )

  (apply #'set-background bg-main)
  (apply #'set-foreground fg-main)

  ;; All 22 of them. Anything skipped keeps the last theme's colour *and
  ;; weight*, which is the half that bites: an unset face inheriting a bold from
  ;; the theme before it looks like a highlighter bug, not a missing line.
  (set-face "default"           fg-main)                ; #ffffff  Modus `fg-main'
  (set-face "keyword"           magenta-cooler)         ; #b6a0ff  Modus `keyword'
  (set-face "function"          magenta)                ; #feacd0  Modus `fnname'
  (set-face "type"              cyan-cooler)            ; #6ae4b9  Modus `type'
  (set-face "string"            blue-warmer)            ; #79a8ff  Modus `string'
  (set-face "number"            yellow-faint)           ; #d2b580  Modus `identifier'
  (set-face "comment"           fg-dim)                 ; #989898  Modus `comment'
  (set-face "constant"          blue-cooler)            ; #00bcff  Modus `constant'
  (set-face "variable"          cyan)                   ; #00d3d0  Modus `variable'
  (set-face "operator"          fg-main)                ; #ffffff  Modus `operator'
  (set-face "punctuation"       fg-main)                ; #ffffff  Modus `punctuation'
  (set-face "heading-1"         fg-main       :bold t)  ; #ffffff  Modus `fg-heading-1'
  (set-face "heading-2"         yellow-faint  :bold t)  ; #d2b580  Modus `fg-heading-2'
  (set-face "heading-3"         blue-faint    :bold t)  ; #82b0ec  Modus `fg-heading-3'
  (set-face "bold"              magenta-warmer :bold t) ; #f78fe7  Modus `fg-prose-verbatim'
  (set-face "italic"            green-faint :italic t)  ; #88ca9f  Modus `docstring'
  (set-face "link"              blue-warmer)            ; #79a8ff  Modus `fg-link'
  (set-face "code"              cyan-cooler)            ; #6ae4b9  Modus `fg-prose-code'
  (set-face "markup"            fg-dim)                 ; #989898  Modus `prose-metadata'
  (set-face "modeline"          bg-mode-line-active)    ; #505050  Modus `bg-mode-line-active'
  (set-face "modeline-inactive" bg-mode-line-inactive)  ; #2d2d2d  Modus `bg-mode-line-inactive'
  (set-face "modeline-text"     fg-main)                ; #ffffff  Modus `fg-mode-line-active'

  ;; The twelve optional ones. Left unset they fall back to a ratio the renderer
  ;; mixes off the ground, which is a guess in the one theme that measured every
  ;; value it publishes.
  (set-face "region"              bg-region)          ; #5a5a5a  Modus `bg-region'
  (set-face "cursor"              fg-main)            ; #ffffff  Modus `cursor'
  (set-face "current-line"        bg-hl-line)         ; #2f3849  Modus `bg-hl-line'
  (set-face "line-number"         fg-dim)             ; #989898  Modus `fg-line-number-inactive'
  (set-face "line-number-current" fg-main)            ; #ffffff  Modus `fg-line-number-active'
  (set-face "divider"             border)             ; #646464  Modus `border'
  (set-face "error"               red)                ; #ff5f59  Modus `err'
  (set-face "warning"             yellow-warmer)      ; #fec43f  Modus `warning'
  (set-face "match"               bg-yellow-intense)  ; #7a6100  Modus `bg-search-current'
  (set-face "popup"               bg-popup)           ; #0c0c0c  Modus `bg-popup'
  (set-face "popup-border"        border)             ; #646464  Modus `child-frame-border'
  (set-face "accent"              blue-cooler)        ; #00bcff  Modus `accent-0'
  )
