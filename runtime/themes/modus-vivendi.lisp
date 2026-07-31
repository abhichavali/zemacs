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
;;;; * `heading-1' — Modus' `fg-heading-1' is `fg-main' and leans on bold weight
;;;;   to carry the level. zemacs opens one font face and has no bold, so its
;;;;   three heading levels take Modus' three *coloured* ones: 0, 2 and 3.
;;;; * `bold' and `italic' — the same problem. They take the colours Modus gives
;;;;   the nearest thing it does colour, `fg-prose-verbatim' and `docstring'.

(in-package :zemacs)

;;; The palette, named as Modus names it.
(let (;; Basic values
      (bg-main               '(0.000 0.000 0.000))  ; #000000
      (fg-main               '(1.000 1.000 1.000))  ; #ffffff
      (fg-dim                '(0.596 0.596 0.596))  ; #989898

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
      (yellow-faint          '(0.824 0.710 0.502))  ; #d2b580

      ;; Modeline backgrounds
      (bg-mode-line-active   '(0.314 0.314 0.314))  ; #505050
      (bg-mode-line-inactive '(0.176 0.176 0.176))  ; #2d2d2d
      )

  (apply #'set-background bg-main)
  (apply #'set-foreground fg-main)

  ;; All 22 of them. Anything skipped keeps the last theme's colour.
  (flet ((face (name rgb) (apply #'set-syntax-color name rgb)))
    (face "default"           fg-main)                ; #ffffff  Modus `fg-main'
    (face "keyword"           magenta-cooler)         ; #b6a0ff  Modus `keyword'
    (face "function"          magenta)                ; #feacd0  Modus `fnname'
    (face "type"              cyan-cooler)            ; #6ae4b9  Modus `type'
    (face "string"            blue-warmer)            ; #79a8ff  Modus `string'
    (face "number"            yellow-faint)           ; #d2b580  Modus `identifier'
    (face "comment"           fg-dim)                 ; #989898  Modus `comment'
    (face "constant"          blue-cooler)            ; #00bcff  Modus `constant'
    (face "variable"          cyan)                   ; #00d3d0  Modus `variable'
    (face "operator"          fg-main)                ; #ffffff  Modus `operator'
    (face "punctuation"       fg-main)                ; #ffffff  Modus `punctuation'
    (face "heading-1"         cyan-cooler)            ; #6ae4b9  Modus `fg-heading-0'
    (face "heading-2"         yellow-faint)           ; #d2b580  Modus `fg-heading-2'
    (face "heading-3"         blue-faint)             ; #82b0ec  Modus `fg-heading-3'
    (face "bold"              magenta-warmer)         ; #f78fe7  Modus `fg-prose-verbatim'
    (face "italic"            green-faint)            ; #88ca9f  Modus `docstring'
    (face "link"              blue-warmer)            ; #79a8ff  Modus `fg-link'
    (face "code"              cyan-cooler)            ; #6ae4b9  Modus `fg-prose-code'
    (face "markup"            fg-dim)                 ; #989898  Modus `prose-metadata'
    (face "modeline"          bg-mode-line-active)    ; #505050  Modus `bg-mode-line-active'
    (face "modeline-inactive" bg-mode-line-inactive)  ; #2d2d2d  Modus `bg-mode-line-inactive'
    (face "modeline-text"     fg-main)                ; #ffffff  Modus `fg-mode-line-active'
    ))
