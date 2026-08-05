;;;; gruvbox-dark — Pavel Pertsev's retro groove, warm and heavy
;;;;
;;;; Gruvbox is built from a ramp and seven hues. The ramp runs `dark0'
;;;; (#282828, the ground here) up through the greys to `light0'; the hues come
;;;; in a `bright', a `neutral' and a `faded' step, and which step you use is
;;;; decided by the background you draw on. Dark gruvbox takes the bright ones.
;;;; Every hex below is the published value, bound under Pertsev's own names.
;;;;
;;;; Load it from your init.lisp — these are ordinary top-level forms, so
;;;; loading the file *is* applying the theme:
;;;;
;;;;   (load-theme "gruvbox-dark")
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
;;;; On weight: gruvbox is the theme that leans hardest on bold, and it is worth
;;;; being clear about where the bold comes from, because the two upstreams put
;;;; it behind different switches. vim gruvbox bolds function names outright
;;;; (`GruvboxGreenBold') and headings with them. gruvbox-theme.el writes its
;;;; keyword and org-level faces as `:bold gruvbox-bold-constructs' and then
;;;; ships that variable NIL, so the bold is offered rather than applied. This
;;;; port takes the offer: six faces are bold and no others.
;;;;
;;;; Four mappings are not either upstream's own:
;;;;
;;;; * `function' and `type' — here the two upstreams disagree. gruvbox-theme.el
;;;;   paints functions `bright_yellow' and types `bright_purple', which lands
;;;;   `type', `constant' and `number' on one indistinguishable colour. vim
;;;;   gruvbox, which that port follows nearly everywhere else, paints functions
;;;;   `GruvboxGreenBold' — the same green as strings, separated by weight alone
;;;;   — and types `GruvboxYellow'. zemacs could not say "same hue, heavier"
;;;;   until faces learned weight; it can now, so it takes vim's reading and
;;;;   keeps four distinct hues across function, type, constant and variable.
;;;; * `bold' and `italic' — org's own emphasis faces carry no colour in either
;;;;   upstream; vim gives `markdownItalic' the `fg3' grey and a slant and stops
;;;;   there. So `italic' is that, and `bold' is `light0', one step brighter
;;;;   than the body text, plus the weight. The Modus files spend a hue on these
;;;;   two because the old API could only speak in hues. This one need not.
;;;; * `link' — gruvbox-theme.el's `link' face is #458588, 3.5:1 against `dark0'
;;;;   and too dim for something you are meant to follow. It is lifted to the
;;;;   bright step of the same blue, which puts it on `variable' and
;;;;   `heading-1' as well. Seven hues do not cover 22 faces; these three never
;;;;   share a buffer.
;;;; * `punctuation' — neither upstream links a generic `Delimiter'. It takes
;;;;   `light3', the step vim spends on markdown's structural delimiters.

(in-package :zemacs)

;;; The palette, named as Pertsev names it.
(let (;; The ramp. `dark0' is the medium-contrast ground; the hard and soft
      ;; variants move only this one value.
      (dark0         '(0.157 0.157 0.157))  ; #282828
      (dark1         '(0.235 0.220 0.212))  ; #3c3836
      (dark3         '(0.400 0.361 0.329))  ; #665c54
      (gray          '(0.573 0.514 0.455))  ; #928374
      (light3        '(0.741 0.682 0.576))  ; #bdae93
      (light2        '(0.835 0.769 0.631))  ; #d5c4a1
      (light1        '(0.922 0.859 0.698))  ; #ebdbb2
      (light0        '(0.984 0.945 0.780))  ; #fbf1c7

      ;; The bright accent step, which is the one a dark ground calls for. The
      ;; neutral and faded steps below it belong to the light theme and to
      ;; diffs, and none of them appear here. (gruvbox-theme.el writes bright
      ;; red as #fb4933, one unit off Pertsev's #fb4934; the original wins.)
      (bright-red    '(0.984 0.286 0.204))  ; #fb4934
      (bright-green  '(0.722 0.733 0.149))  ; #b8bb26
      (bright-yellow '(0.980 0.741 0.184))  ; #fabd2f
      (bright-blue   '(0.514 0.647 0.596))  ; #83a598
      (bright-purple '(0.827 0.525 0.608))  ; #d3869b
      (bright-aqua   '(0.557 0.753 0.486))  ; #8ec07c
      (bright-orange '(0.996 0.502 0.098))  ; #fe8019
      )

  (apply #'set-background dark0)
  (apply #'set-foreground light1)

  ;; All 22 of them. Anything skipped keeps the last theme's colour and weight.
  (set-face "default"           light1)                 ; #ebdbb2  `Normal'
  (set-face "keyword"           bright-red :bold t)     ; #fb4934  `font-lock-keyword-face'
  (set-face "function"          bright-green :bold t)   ; #b8bb26  `GruvboxGreenBold'
  (set-face "type"              bright-yellow)          ; #fabd2f  `Type'
  (set-face "string"            bright-green)           ; #b8bb26  `String'
  (set-face "number"            bright-purple)          ; #d3869b  `Number'
  (set-face "comment"           gray :italic t)         ; #928374  `Comment'
  (set-face "constant"          bright-purple)          ; #d3869b  `Constant'
  (set-face "variable"          bright-blue)            ; #83a598  `Identifier'
  (set-face "operator"          light1)                 ; #ebdbb2  `Operator' -> `Normal'
  (set-face "punctuation"       light3)                 ; #bdae93  vim `fg3'
  (set-face "heading-1"         bright-blue :bold t)    ; #83a598  `org-level-1'
  (set-face "heading-2"         bright-yellow :bold t)  ; #fabd2f  `org-level-2'
  (set-face "heading-3"         bright-purple :bold t)  ; #d3869b  `org-level-3'
  (set-face "bold"              light0 :bold t)         ; #fbf1c7  org emphasis
  (set-face "italic"            light3 :italic t)       ; #bdae93  `markdownItalic'
  (set-face "link"              bright-blue)            ; #83a598  `link', lifted
  (set-face "code"              bright-aqua)            ; #8ec07c  `markdownCode'
  (set-face "markup"            bright-orange)          ; #fe8019  `markdownHeadingDelimiter'
  (set-face "modeline"          dark3)                  ; #665c54  `mode-line' bg
  (set-face "modeline-inactive" dark1)                  ; #3c3836  `mode-line-inactive' bg
  (set-face "modeline-text"     light2)                 ; #d5c4a1  `mode-line' fg
  )
