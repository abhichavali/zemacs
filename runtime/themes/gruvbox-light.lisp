;;;; gruvbox-light — the same retro groove, read off paper instead of a screen
;;;;
;;;; Gruvbox is built from a ramp and seven hues. The ramp runs `dark0' up
;;;; through the greys to `light0' (#fbf1c7, the ground here, a warm cream and
;;;; not a white); the hues each come in a `bright', a `neutral' and a `faded'
;;;; step, and which step you use is decided by the background you are drawing
;;;; on. Light gruvbox takes the faded ones, and it is a different accent set
;;;; rather than the dark theme inverted — #9d0006 is not #fb4934 turned around.
;;;; Every hex below is the published value, bound under Pertsev's own names.
;;;;
;;;; (Note for anyone comparing against gruvbox-theme.el: that port implements
;;;; the light themes by *relabelling* the palette, so its `dark0' is #fbf1c7
;;;; and its `bright_red' is #9d0006. The values are the same, the labels move.
;;;; This file keeps Pertsev's labels, so `light0' is the light one here.)
;;;;
;;;; Load it from your init.lisp — these are ordinary top-level forms, so
;;;; loading the file *is* applying the theme:
;;;;
;;;;   (load-theme "gruvbox-light")
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. A face left out here is a face the previous theme still owns
;;;; — its colour *and*, now that faces carry weight, its bold and its slant.
;;;; That is the whole reason the list is exhaustive and boring.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse
;;;; these. A directory is `type', an executable `function', a symlink `link',
;;;; a staged file `string' and an unstaged one `keyword'. So the same 22 lines
;;;; that colour a source buffer colour those too.
;;;;
;;;; On weight: the six bold faces and two italic ones are the same six and two
;;;; as in gruvbox-dark, for the reasons that file gives at length — vim bolds
;;;; function names and headings outright, gruvbox-theme.el offers keyword and
;;;; org-level bold behind `gruvbox-bold-constructs' and ships it NIL, and this
;;;; port takes the offer. Weight is the one thing a theme can change about a
;;;; light background without spending contrast it does not have.
;;;;
;;;; Four mappings are not either upstream's own:
;;;;
;;;; * `function' and `type' — the two upstreams disagree, and this file settles
;;;;   it the same way gruvbox-dark does: functions take vim's green (bold, the
;;;;   same green as strings, separated by weight alone) and types take vim's
;;;;   yellow, rather than gruvbox-theme.el's yellow functions and purple types
;;;;   which would put `type', `constant' and `number' on one colour.
;;;; * `comment' — vim gruvbox comments in `gray' (#928374) whichever ground it
;;;;   is on. On `dark0' that is 4.1:1 and fine; on `light0' it is 3.1:1, which
;;;;   is a comment you have to hunt for. So the light theme steps one down the
;;;;   dark end of the ramp to `dark4' (4.3:1) and keeps the slant.
;;;; * `bold' and `italic' — org's own emphasis faces carry no colour in either
;;;;   upstream; vim gives `markdownItalic' the `fg3' grey and a slant and stops
;;;;   there. So `italic' is that (here `dark3'), and `bold' is `dark0', a step
;;;;   darker than the body text, plus the weight.
;;;; * `link' — gruvbox-theme.el's `link' is #458588, 3.7:1 on `light0' and too
;;;;   pale for something you are meant to follow; it takes the light set's own
;;;;   blue instead, which puts it on `variable' and `heading-1' as well. Seven
;;;;   hues do not cover 22 faces, and those three never share a buffer.
;;;;
;;;; One face is under 4.5:1 and stays there on purpose. `faded-yellow'
;;;; (#b57614) is 3.3:1 on `light0', and it is the darkest yellow Pertsev
;;;; published — there is no lower step to move `type' to. Upstream lives with
;;;; it, and inventing a hue to fix it would stop this being gruvbox.

(in-package :zemacs)

;;; The palette, named as Pertsev names it.
(let (;; The ramp. `light0' is the medium-contrast ground; the hard and soft
      ;; variants move only this one value.
      (light0        '(0.984 0.945 0.780))  ; #fbf1c7
      (light1        '(0.922 0.859 0.698))  ; #ebdbb2
      (light3        '(0.741 0.682 0.576))  ; #bdae93
      (dark4         '(0.486 0.435 0.392))  ; #7c6f64
      (dark3         '(0.400 0.361 0.329))  ; #665c54
      (dark2         '(0.314 0.286 0.271))  ; #504945
      (dark1         '(0.235 0.220 0.212))  ; #3c3836
      (dark0         '(0.157 0.157 0.157))  ; #282828

      ;; The faded accent step, which is the one a light ground calls for. These
      ;; are not the dark theme's accents darkened; they are their own published
      ;; set, and the difference is most obvious in red.
      (faded-red     '(0.616 0.000 0.024))  ; #9d0006
      (faded-green   '(0.475 0.455 0.055))  ; #79740e
      (faded-yellow  '(0.710 0.463 0.078))  ; #b57614
      (faded-blue    '(0.027 0.400 0.471))  ; #076678
      (faded-purple  '(0.561 0.247 0.443))  ; #8f3f71
      (faded-aqua    '(0.259 0.482 0.345))  ; #427b58
      (faded-orange  '(0.686 0.227 0.012))  ; #af3a03
      )

  (apply #'set-background light0)
  (apply #'set-foreground dark1)

  ;; All 22 of them. Anything skipped keeps the last theme's colour and weight.
  (set-face "default"           dark1)                  ; #3c3836  `Normal'
  (set-face "keyword"           faded-red :bold t)      ; #9d0006  `font-lock-keyword-face'
  (set-face "function"          faded-green :bold t)    ; #79740e  `GruvboxGreenBold'
  (set-face "type"              faded-yellow)           ; #b57614  `Type'
  (set-face "string"            faded-green)            ; #79740e  `String'
  (set-face "number"            faded-purple)           ; #8f3f71  `Number'
  (set-face "comment"           dark4 :italic t)        ; #7c6f64  `Comment', darkened
  (set-face "constant"          faded-purple)           ; #8f3f71  `Constant'
  (set-face "variable"          faded-blue)             ; #076678  `Identifier'
  (set-face "operator"          dark1)                  ; #3c3836  `Operator' -> `Normal'
  (set-face "punctuation"       dark3)                  ; #665c54  vim `fg3'
  (set-face "heading-1"         faded-blue :bold t)     ; #076678  `org-level-1'
  (set-face "heading-2"         faded-yellow :bold t)   ; #b57614  `org-level-2'
  (set-face "heading-3"         faded-purple :bold t)   ; #8f3f71  `org-level-3'
  (set-face "bold"              dark0 :bold t)          ; #282828  org emphasis
  (set-face "italic"            dark3 :italic t)        ; #665c54  `markdownItalic'
  (set-face "link"              faded-blue)             ; #076678  `link', darkened
  (set-face "code"              faded-aqua)             ; #427b58  `markdownCode'
  (set-face "markup"            faded-orange)           ; #af3a03  `markdownHeadingDelimiter'
  (set-face "modeline"          light3)                 ; #bdae93  `mode-line' bg
  (set-face "modeline-inactive" light1)                 ; #ebdbb2  `mode-line-inactive' bg
  (set-face "modeline-text"     dark2)                  ; #504945  `mode-line' fg
  )
