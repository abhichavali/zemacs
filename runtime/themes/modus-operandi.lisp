;;;; modus-operandi — elegant, highly legible theme with a white background
;;;;
;;;; Protesilaos Stavrou's Modus themes, ported to zemacs. Their point is
;;;; contrast: every colour below is at least 7:1 against the background it is
;;;; drawn on, which is the WCAG AAA threshold. The values are the published
;;;; ones from `modus-themes-operandi-palette', not approximations of them, so
;;;; that property survives the port.
;;;;
;;;; Load it from your init.lisp — these are ordinary top-level forms, so
;;;; loading the file *is* applying the theme:
;;;;
;;;;   (load (merge-pathnames "themes/modus-operandi.lisp" *load-truename*))
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
;;;; deliberately does not shout — which on a white ground matters more than it
;;;; does on a dark one, since bold black text is the heaviest thing a light
;;;; theme can put on a screen.
;;;;
;;;; What you actually see may still be heavier, because `*bold-constructs*' in
;;;; init.lisp is applied on top of whichever theme is loaded and bolds
;;;; keywords, functions and types by default. Set it to NIL for this theme as
;;;; Protesilaos published it — and this is the theme most worth doing that for,
;;;; for the reason in the paragraph above.
;;;;
;;;; The twelve optional UI faces are Modus' semantic mappings read straight
;;;; down: `bg-region', `cursor', `bg-hl-line', the two `fg-line-number-'
;;;; entries, `err', `warning', `bg-popup' — one fill here for the several
;;;; child frames Modus dresses off it — `accent-0' of the four it grades, and
;;;; `border', which upstream gives the divider and the popup stroke both.
;;;; Two are this port's judgement: `match' takes `bg-search-current' over the
;;;; lazier cyan, one band standing in for every hit and cyan sinking into the
;;;; bluish current line; and `line-number-current' is plain where upstream's
;;;; is bold, that bold being there to carry a gutter background zemacs has no
;;;; face to set.

(in-package :zemacs)

;;; The palette, named as Modus names it.
(let (;; Basic values
      (bg-main               '(1.000 1.000 1.000))  ; #ffffff
      (fg-main               '(0.000 0.000 0.000))  ; #000000
      (fg-dim                '(0.349 0.349 0.349))  ; #595959
      (fg-alt                '(0.098 0.212 0.408))  ; #193668
      (border                '(0.624 0.624 0.624))  ; #9f9f9f

      ;; Accent foregrounds
      (blue                  '(0.000 0.192 0.663))  ; #0031a9
      (blue-warmer           '(0.208 0.282 0.812))  ; #3548cf
      (blue-cooler           '(0.000 0.000 0.690))  ; #0000b0
      (cyan                  '(0.000 0.369 0.545))  ; #005e8b
      (cyan-cooler           '(0.000 0.373 0.373))  ; #005f5f
      (green-faint           '(0.165 0.314 0.271))  ; #2a5045
      (magenta               '(0.447 0.063 0.271))  ; #721045
      (magenta-warmer        '(0.561 0.000 0.459))  ; #8f0075
      (magenta-cooler        '(0.325 0.102 0.714))  ; #531ab6
      (red                   '(0.651 0.000 0.000))  ; #a60000
      (yellow-faint          '(0.384 0.267 0.086))  ; #624416
      (yellow-cooler         '(0.478 0.310 0.184))  ; #7a4f2f
      (yellow-warmer         '(0.533 0.286 0.000))  ; #884900

      ;; UI backgrounds
      (bg-region             '(0.741 0.741 0.741))  ; #bdbdbd
      (bg-hl-line            '(0.855 0.898 0.925))  ; #dae5ec
      (bg-popup              '(0.953 0.953 0.953))  ; #f3f3f3
      (bg-yellow-intense     '(0.953 0.816 0.000))  ; #f3d000

      ;; Modeline backgrounds
      (bg-mode-line-active   '(0.784 0.784 0.784))  ; #c8c8c8
      (bg-mode-line-inactive '(0.902 0.902 0.902))  ; #e6e6e6
      )

  (apply #'set-background bg-main)
  (apply #'set-foreground fg-main)

  ;; All 22 of them. Anything skipped keeps the last theme's colour *and
  ;; weight*, which is the half that bites: an unset face inheriting a bold from
  ;; the theme before it looks like a highlighter bug, not a missing line.
  (set-face "default"           fg-main)                ; #000000  Modus `fg-main'
  (set-face "keyword"           magenta-cooler)         ; #531ab6  Modus `keyword'
  (set-face "function"          magenta)                ; #721045  Modus `fnname'
  (set-face "type"              cyan-cooler)            ; #005f5f  Modus `type'
  (set-face "string"            blue-warmer)            ; #3548cf  Modus `string'
  (set-face "number"            yellow-cooler)          ; #7a4f2f  Modus `identifier'
  (set-face "comment"           fg-dim)                 ; #595959  Modus `comment'
  (set-face "constant"          blue-cooler)            ; #0000b0  Modus `constant'
  (set-face "variable"          cyan)                   ; #005e8b  Modus `variable'
  (set-face "operator"          fg-main)                ; #000000  Modus `operator'
  (set-face "punctuation"       fg-main)                ; #000000  Modus `punctuation'
  (set-face "heading-1"         fg-main       :bold t)  ; #000000  Modus `fg-heading-1'
  (set-face "heading-2"         yellow-faint  :bold t)  ; #624416  Modus `fg-heading-2'
  (set-face "heading-3"         fg-alt        :bold t)  ; #193668  Modus `fg-heading-3'
  (set-face "bold"              magenta-warmer :bold t) ; #8f0075  Modus `fg-prose-verbatim'
  (set-face "italic"            green-faint :italic t)  ; #2a5045  Modus `docstring'
  (set-face "link"              blue-warmer)            ; #3548cf  Modus `fg-link'
  (set-face "code"              cyan-cooler)            ; #005f5f  Modus `fg-prose-code'
  (set-face "markup"            fg-dim)                 ; #595959  Modus `prose-metadata'
  (set-face "modeline"          bg-mode-line-active)    ; #c8c8c8  Modus `bg-mode-line-active'
  (set-face "modeline-inactive" bg-mode-line-inactive)  ; #e6e6e6  Modus `bg-mode-line-inactive'
  (set-face "modeline-text"     fg-main)                ; #000000  Modus `fg-mode-line-active'

  ;; The optional twelve, which this port sets rather than leave to the
  ;; renderer's ratios — a mixed ground is the one thing that cannot be
  ;; guaranteed AAA, and Modus publishes every one of these outright.
  (set-face "region"               bg-region)           ; #bdbdbd  Modus `bg-region'
  (set-face "cursor"               fg-main)             ; #000000  Modus `cursor'
  (set-face "current-line"         bg-hl-line)          ; #dae5ec  Modus `bg-hl-line'
  (set-face "line-number"          fg-dim)              ; #595959  Modus `fg-line-number-inactive'
  (set-face "line-number-current"  fg-main)             ; #000000  Modus `fg-line-number-active'
  (set-face "divider"              border)              ; #9f9f9f  Modus `border'
  (set-face "error"                red)                 ; #a60000  Modus `err'
  (set-face "warning"              yellow-warmer)       ; #884900  Modus `warning'
  (set-face "match"                bg-yellow-intense)   ; #f3d000  Modus `bg-search-current'
  (set-face "popup"                bg-popup)            ; #f3f3f3  Modus `bg-popup'
  (set-face "popup-border"         border)              ; #9f9f9f  Modus `border'
  (set-face "accent"               blue)                ; #0031a9  Modus `accent-0'
  )
