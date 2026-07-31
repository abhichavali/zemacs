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
;;;; * `heading-1' — Modus' `fg-heading-1' is `fg-main' and leans on bold weight
;;;;   to carry the level. zemacs opens one font face and has no bold, so its
;;;;   three heading levels take Modus' three *coloured* ones: 0, 2 and 3.
;;;; * `bold' and `italic' — the same problem. They take the colours Modus gives
;;;;   the nearest thing it does colour, `fg-prose-verbatim' and `docstring'.

(in-package :zemacs)

;;; The palette, named as Modus names it.
(let (;; Basic values
      (bg-main               '(1.000 1.000 1.000))  ; #ffffff
      (fg-main               '(0.000 0.000 0.000))  ; #000000
      (fg-dim                '(0.349 0.349 0.349))  ; #595959
      (fg-alt                '(0.098 0.212 0.408))  ; #193668

      ;; Accent foregrounds
      (blue-warmer           '(0.208 0.282 0.812))  ; #3548cf
      (blue-cooler           '(0.000 0.000 0.690))  ; #0000b0
      (cyan                  '(0.000 0.369 0.545))  ; #005e8b
      (cyan-cooler           '(0.000 0.373 0.373))  ; #005f5f
      (green-faint           '(0.165 0.314 0.271))  ; #2a5045
      (magenta               '(0.447 0.063 0.271))  ; #721045
      (magenta-warmer        '(0.561 0.000 0.459))  ; #8f0075
      (magenta-cooler        '(0.325 0.102 0.714))  ; #531ab6
      (yellow-faint          '(0.384 0.267 0.086))  ; #624416
      (yellow-cooler         '(0.478 0.310 0.184))  ; #7a4f2f

      ;; Modeline backgrounds
      (bg-mode-line-active   '(0.784 0.784 0.784))  ; #c8c8c8
      (bg-mode-line-inactive '(0.902 0.902 0.902))  ; #e6e6e6
      )

  (apply #'set-background bg-main)
  (apply #'set-foreground fg-main)

  ;; All 22 of them. Anything skipped keeps the last theme's colour.
  (flet ((face (name rgb) (apply #'set-syntax-color name rgb)))
    (face "default"           fg-main)                ; #000000  Modus `fg-main'
    (face "keyword"           magenta-cooler)         ; #531ab6  Modus `keyword'
    (face "function"          magenta)                ; #721045  Modus `fnname'
    (face "type"              cyan-cooler)            ; #005f5f  Modus `type'
    (face "string"            blue-warmer)            ; #3548cf  Modus `string'
    (face "number"            yellow-cooler)          ; #7a4f2f  Modus `identifier'
    (face "comment"           fg-dim)                 ; #595959  Modus `comment'
    (face "constant"          blue-cooler)            ; #0000b0  Modus `constant'
    (face "variable"          cyan)                   ; #005e8b  Modus `variable'
    (face "operator"          fg-main)                ; #000000  Modus `operator'
    (face "punctuation"       fg-main)                ; #000000  Modus `punctuation'
    (face "heading-1"         cyan-cooler)            ; #005f5f  Modus `fg-heading-0'
    (face "heading-2"         yellow-faint)           ; #624416  Modus `fg-heading-2'
    (face "heading-3"         fg-alt)                 ; #193668  Modus `fg-heading-3'
    (face "bold"              magenta-warmer)         ; #8f0075  Modus `fg-prose-verbatim'
    (face "italic"            green-faint)            ; #2a5045  Modus `docstring'
    (face "link"              blue-warmer)            ; #3548cf  Modus `fg-link'
    (face "code"              cyan-cooler)            ; #005f5f  Modus `fg-prose-code'
    (face "markup"            fg-dim)                 ; #595959  Modus `prose-metadata'
    (face "modeline"          bg-mode-line-active)    ; #c8c8c8  Modus `bg-mode-line-active'
    (face "modeline-inactive" bg-mode-line-inactive)  ; #e6e6e6  Modus `bg-mode-line-inactive'
    (face "modeline-text"     fg-main)                ; #000000  Modus `fg-mode-line-active'
    ))
