;;;; dracula — the dark theme everything has a port of
;;;;
;;;; Zeno Rocha's Dracula: eleven colours that turn up identically in a few
;;;; hundred editors, which is most of the point of it. The values below are the
;;;; published specification ones — Background #282a36, Foreground #f8f8f2 and
;;;; the nine accents — and the *assignments* are lifted from `dracula-theme.el'
;;;; rather than chosen by eye, so a buffer here looks like a buffer there.
;;;;
;;;; Dracula is the loud one. Where Modus spends its budget on contrast, Dracula
;;;; spends it on weight: `dracula-bolder-keywords' ships on and bolds keywords
;;;; and function names, `font-lock-type-face' inherits the italic builtin face,
;;;; and org's first two heading levels inherit `bold'. zemacs could not say any
;;;; of that until faces grew a weight, so this port says all of it.
;;;;
;;;;   (load-theme "dracula")
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. A face left out here is a face the previous theme still
;;;; owns — its colour *and now its weight*, which is the worse half — and that
;;;; is the whole reason the list is exhaustive and boring.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse
;;;; these. A directory is `type', an executable `function', a symlink `link',
;;;; a staged file `string' and an unstaged one `keyword'. So the same lines
;;;; that colour a source buffer colour those too.
;;;;
;;;; Six mappings are not upstream's own:
;;;;
;;;; * `variable' — upstream bolds `font-lock-variable-name-face' alongside
;;;;   keywords and function names. The colour is ported and the weight is not.
;;;;   Identifiers are the commonest token on a screen, and bolding them spends
;;;;   exactly the emphasis that bolding keywords was buying.
;;;; * `heading-3' — `org-level-3' is normal weight upstream and carries the
;;;;   level with `:height' instead. zemacs opens one font at one size, so the
;;;;   third level takes bold like the first two and hue does the ranking.
;;;; * `bold' and `italic' — `dracula-theme.el' leaves org's `*bold*' and
;;;;   `/italic/' to Emacs' own `bold' and `italic' faces, which carry weight
;;;;   and no colour at all. The specification does name them, so they take its
;;;;   answer: MarkupBold is orange-bold and MarkupItalic is yellow-italic.
;;;; * `punctuation' — Dracula has no font-lock face for it and Emacs draws it
;;;;   in the default foreground. So does this.
;;;; * `link' — upstream is cyan *and* underlined. zemacs does not underline,
;;;;   so the cyan carries the whole signal on its own.
;;;; * `modeline-inactive' — upstream's inactive mode line is the background
;;;;   colour itself, told apart from the buffer only by a `:box'. zemacs draws
;;;;   no box, so a bar painted that way would simply stop being a bar. It
;;;;   takes `dracula-current' (#353747) instead, the port's own darker
;;;;   current-line value, and stays visible.
;;;;
;;;; Comments are not italic, which surprises people who expect Dracula to
;;;; italicise everything. `font-lock-comment-face' inherits `shadow', which
;;;; sets a colour and nothing else, and the specification asks for no slant
;;;; there either. The italics Dracula does ask for are on types and on markup
;;;; emphasis, and both of those are here.

(in-package :zemacs)

;;; The palette, named as `dracula-theme.el' names it.
(let (;; The official eleven
      (dracula-bg      '(0.157 0.165 0.212))  ; #282a36  spec Background
      (dracula-fg      '(0.973 0.973 0.949))  ; #f8f8f2  spec Foreground
      (dracula-comment '(0.384 0.447 0.643))  ; #6272a4  spec Comment
      (dracula-cyan    '(0.545 0.914 0.992))  ; #8be9fd
      (dracula-green   '(0.314 0.980 0.482))  ; #50fa7b
      (dracula-orange  '(1.000 0.722 0.424))  ; #ffb86c
      (dracula-pink    '(1.000 0.475 0.776))  ; #ff79c6
      (dracula-purple  '(0.741 0.576 0.976))  ; #bd93f9
      (dracula-red     '(1.000 0.333 0.333))  ; #ff5555
      (dracula-yellow  '(0.945 0.980 0.549))  ; #f1fa8c

      ;; Ground furniture. The specification gives Current Line and Selection
      ;; the same value and the port calls it `dracula-region'; `dracula-current'
      ;; is the port's own darker current-line fallback, which is the only thing
      ;; in the palette dark enough to be an inactive mode line and still a bar.
      (dracula-region  '(0.267 0.278 0.353))  ; #44475a
      (dracula-current '(0.208 0.216 0.278))  ; #353747
      )
  ;; zemacs has no error or warning face, so the red goes unspent. It is bound
  ;; anyway: the palette above is worth checking against the specification line
  ;; by line, and a hole in the middle of it is one more thing to wonder about.
  (declare (ignorable dracula-red))

  (apply #'set-background dracula-bg)
  (apply #'set-foreground dracula-fg)

  ;; All 22 of them. Anything skipped keeps the last theme's colour and weight.
  ;; The trailing name is the `dracula-theme.el' face the mapping came from,
  ;; with the `font-lock-' prefix and the `-face' suffix left off.
  (set-face "default"           dracula-fg)                ; #f8f8f2  default
  (set-face "keyword"           dracula-pink :bold t)      ; #ff79c6  keyword
  (set-face "function"          dracula-green :bold t)     ; #50fa7b  function-name
  (set-face "type"              dracula-cyan :italic t)    ; #8be9fd  type -> builtin
  (set-face "string"            dracula-yellow)            ; #f1fa8c  string
  (set-face "number"            dracula-purple)            ; #bd93f9  number
  (set-face "comment"           dracula-comment)           ; #6272a4  comment -> shadow
  (set-face "constant"          dracula-purple)            ; #bd93f9  constant
  (set-face "variable"          dracula-fg)                ; #f8f8f2  variable-name, unbolded
  (set-face "operator"          dracula-pink)              ; #ff79c6  operator
  (set-face "punctuation"       dracula-fg)                ; #f8f8f2  no upstream face
  (set-face "heading-1"         dracula-pink :bold t)      ; #ff79c6  org-level-1
  (set-face "heading-2"         dracula-purple :bold t)    ; #bd93f9  org-level-2
  (set-face "heading-3"         dracula-green :bold t)     ; #50fa7b  org-level-3, bolded
  (set-face "bold"              dracula-orange :bold t)    ; #ffb86c  spec MarkupBold
  (set-face "italic"            dracula-yellow :italic t)  ; #f1fa8c  spec MarkupItalic
  (set-face "link"              dracula-cyan)              ; #8be9fd  link
  (set-face "code"              dracula-green)             ; #50fa7b  org-code
  (set-face "markup"            dracula-comment)           ; #6272a4  org-document-info-keyword
  (set-face "modeline"          dracula-region)            ; #44475a  mode-line background
  (set-face "modeline-inactive" dracula-current)           ; #353747  see the header
  (set-face "modeline-text"     dracula-fg)                ; #f8f8f2  mode-line foreground
  )
