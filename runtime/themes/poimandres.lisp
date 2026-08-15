;;;; poimandres — pmndrs' gloomy blue, and mint on deep navy
;;;;
;;;; drcmda's theme for the pmndrs collective. The palette below is the one in
;;;; `drcmda/poimandres-theme', src/theme.js — the file the shipped VS Code
;;;; JSON is generated from, so it is the palette rather than a reading of it.
;;;; Its colour names are its own and are not guessable: the mint everyone
;;;; recognises the theme by is brightMint, the body colour is gray, the
;;;; brightest thing on screen is offWhite, and there is no colour called teal
;;;; anywhere in it. They are kept verbatim rather than translated into the
;;;; usual red/green/blue vocabulary, which would have to lie about half.
;;;;
;;;;   (load-theme "poimandres")
;;;;
;;;; Two things the VS Code theme cannot answer, answered elsewhere:
;;;;
;;;; * A ground below the ground. drcmda never darkens; he draws black at an
;;;;   alpha over bg. `olivercederborg/poimandres.nvim' publishes the same
;;;;   palette with the third ground written out, #171922, which is what the
;;;;   unfocused bar uses here.
;;;; * Alpha in general. Much of what this theme does to a background it does
;;;;   with eight bits of opacity, and a zemacs face is opaque. Four bindings
;;;;   are therefore flattened: upstream's colour over bg #1b1e28 at upstream's
;;;;   own opacity, both numbers in the comment so it can be checked. Three are
;;;;   exact; the fourth is `current-line', which upstream draws in the very
;;;;   same #717cb4 at 15% as the selection band. Opaque, the two would be one
;;;;   colour and the band would vanish, so the stripe halves the opacity.
;;;;   Two foregrounds go the other way and drop their alpha rather than
;;;;   flatten it: `comment' is darkerGray at 69% upstream, which composites to
;;;;   #5a5f79 and falls under 3:1 against the ground, and the fence below is
;;;;   worse. Both take the flat colour.
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. Dired and the git status buffer have no faces of their own
;;;; either: a directory is `type', an executable `function', a symlink `link',
;;;; a staged file `string', an unstaged one `keyword'.
;;;;
;;;; What the port decides for itself:
;;;;
;;;; * `keyword' — upstream splits keywords three ways. The catch-all rule for
;;;;   keyword.control paints gray, the body colour; control flow, new, this,
;;;;   super, the storage modifiers and the whole import/export family get
;;;;   brightMint; keyword.operator and storage.type get desaturatedBlue. What
;;;;   tree-sitter hands zemacs under one name is mostly the second group, so
;;;;   brightMint it is — bound to gray this face would never once have
;;;;   differed from `default'.
;;;; * `type' — upstream's entity.name.type is gray at 75%, which composites to
;;;;   something dimmer than body text. This face also paints dired's
;;;;   directories, and a listing below the body colour is the wrong emphasis,
;;;;   so it takes desaturatedBlue, upstream's entity.name.
;;;; * `heading-1' to `heading-3' — upstream holds two answers that disagree:
;;;;   the generic markup.heading scope sits in the brightMint group, and the
;;;;   markdown-specific rule later in the same file overrides it with
;;;;   offWhite. Both are kept, for levels one and two; level three takes
;;;;   lightBlue, upstream's link title and raw block. All three are bold and
;;;;   upstream bolds no heading at all: in a buffer this densely coloured, hue
;;;;   alone does not read as structure.
;;;; * `markup' — the delimiters. Upstream's fence for a fenced block is
;;;;   bluishGray at 31%, which flattened is nearly the ground; the flat colour
;;;;   is used instead, dim but still there.
;;;; * `modeline' and friends — upstream's status bar is drawn on bg and tells
;;;;   nothing apart by it. zemacs writes both bars with one `modeline-text',
;;;;   so the bar has to say which window has focus: focus raises the active
;;;;   one, background3 sinks the rest. The text is bold for the same reason —
;;;;   with one face for the whole bar the choice is all or nothing.
;;;;
;;;; `code' and `link' share a colour, upstream's doing rather than a
;;;; collision: inline code, raw blocks, link titles and textLink are all
;;;; lightBlue. `variable' being brighter than `default' is upstream's too, and
;;;; is the most poimandres thing here — the language is drawn in the gloom and
;;;; your own identifiers are the only things lit.
;;;;
;;;; On slant: the default variant sets fontStyle italic and applies it to
;;;; comments and to the Keyword/Storage scopes, so both are italic here; a
;;;; noitalics variant exists upstream for those who would rather not. `bold'
;;;; and `italic' are bluishGrayBrighter, upstream's own and dimmer than the
;;;; text around them, deliberately.
;;;;
;;;; Unspent: lowerMint #5fb3a1, which is regexes, escapes and CSS attribute
;;;; names; blueishGreen #42675a, sass keywords and JS decorators; and lowerBlue
;;;; #89ddff with pink #f087bd, which between them are the ANSI terminal and
;;;; most of the overview ruler. zemacs has a face for none of it.

(in-package :zemacs)

;;; The palette, named as drcmda names it.
(let (;; Grounds
      (bg            '(0.106 0.118 0.157))  ; #1b1e28  bg
      (background3   '(0.090 0.098 0.133))  ; #171922  the nvim port's
      (focus         '(0.188 0.200 0.251))  ; #303340  focus
      (panel         '(0.125 0.141 0.188))  ; #202430  a literal, upstream

      ;; Grounds upstream states as an alpha, flattened over bg
      (band          '(0.157 0.173 0.235))  ; #282c3c  #717cb4 at 15%
      (stripe        '(0.129 0.145 0.196))  ; #212532  the same, halved
      (found         '(0.251 0.298 0.369))  ; #404c5e  lightBlue at 25%
      (gutter        '(0.220 0.235 0.302))  ; #383c4d  darkerGray at 31%

      ;; Foregrounds
      (gray          '(0.651 0.675 0.804))  ; #a6accd  gray
      (off-white     '(0.894 0.941 0.984))  ; #e4f0fb  offWhite
      (darker-gray   '(0.463 0.486 0.616))  ; #767c9d  darkerGray
      (bluish-gray   '(0.314 0.392 0.467))  ; #506477  bluishGray
      (bluish-lift   '(0.451 0.565 0.667))  ; #7390aa  bluishGrayBrighter

      ;; Accents
      (bright-mint   '(0.365 0.894 0.780))  ; #5de4c7  brightMint
      (light-blue    '(0.678 0.843 1.000))  ; #add7ff  lightBlue
      (desat-blue    '(0.569 0.706 0.835))  ; #91b4d5  desaturatedBlue
      (hot-red       '(0.816 0.404 0.616))  ; #d0679d  hotRed
      (bright-yellow '(1.000 0.980 0.761))  ; #fffac2  brightYellow
      )

  (apply #'set-background bg)
  (apply #'set-foreground gray)

  ;; The 22 core faces. Anything skipped keeps the last theme's colour and
  ;; weight. The trailing note is the upstream scope it came from.
  (set-face "default"        gray)                    ; #a6accd  editor.foreground
  (set-face "keyword"        bright-mint :italic t)   ; #5de4c7  keyword.control.flow
  (set-face "function"       light-blue)              ; #add7ff  entity.name.function
  (set-face "type"           desat-blue)              ; #91b4d5  entity.name
  (set-face "string"         bright-mint)             ; #5de4c7  string
  (set-face "number"         bright-mint)             ; #5de4c7  constant.numeric
  (set-face "comment"        darker-gray :italic t)   ; #767c9d  comment, alpha dropped
  (set-face "constant"       bright-mint)             ; #5de4c7  constant.language
  (set-face "variable"       off-white)               ; #e4f0fb  variable.other
  (set-face "operator"       desat-blue)              ; #91b4d5  keyword.operator
  (set-face "punctuation"    gray)                    ; #a6accd  meta.brace
  (set-face "heading-1"      bright-mint :bold t)     ; #5de4c7  markup.heading
  (set-face "heading-2"      off-white   :bold t)     ; #e4f0fb  markdown.heading
  (set-face "heading-3"      light-blue  :bold t)     ; #add7ff  see the header
  (set-face "bold"           bluish-lift :bold t)     ; #7390aa  markup.bold
  (set-face "italic"         bluish-lift :italic t)   ; #7390aa  markup.italic
  (set-face "link"           light-blue)              ; #add7ff  textLink.foreground
  (set-face "code"           light-blue)              ; #add7ff  markup.inline.raw
  (set-face "markup"         bluish-gray)             ; #506477  the fence, flattened
  (set-face "modeline"       focus)                   ; #303340  bar, current window
  (set-face "modeline-inactive" background3)          ; #171922  bar, other windows
  (set-face "modeline-text"  gray        :bold t)     ; #a6accd  statusBar.foreground

  ;; The 12 UI faces. Optional in general — each falls back to a mix of the
  ;; ground and the body colour — but this theme names them.
  (set-face "region"         band)                    ; #282c3c  editor.selection
  (set-face "cursor"         gray)                    ; #a6accd  editorCursor
  (set-face "current-line"   stripe)                  ; #212532  lineHighlight, halved
  (set-face "line-number"    gutter)                  ; #383c4d  editorLineNumber
  (set-face "line-number-current" gray)               ; #a6accd  its activeForeground
  (set-face "divider"        focus)                   ; #303340  tree.indentGuidesStroke
  (set-face "error"          hot-red)                 ; #d0679d  editorError
  (set-face "warning"        bright-yellow)           ; #fffac2  editorWarning
  (set-face "match"          found)                   ; #404c5e  editor.findMatch
  (set-face "popup"          panel)                   ; #202430  editorHoverWidget bar
  (set-face "popup-border"   focus)                   ; #303340  notifications.border
  (set-face "accent"         bright-mint)             ; #5de4c7  suggestWidget.highlightFg
  )
