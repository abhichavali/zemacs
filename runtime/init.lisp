;;; zemacs default configuration — Common Lisp.
;;;;
;;;; This file is LOADed on startup by the embedded ECL image, which runs on its
;;;; own thread with its own GC. It is the config file the way ~/.emacs is for
;;;; Emacs: everything below is ordinary Common Lisp, evaluated at startup, and
;;;; anything you can express in CL you can express here.
;;;;
;;;; If a form in this file signals an error, the error is caught and shown in
;;;; the status line — the editor still starts, it just stops loading here.
;;;;
;;;; Host primitives live in the ZEMACS package.
;;;;
;;;;   (set-font-size n)                       point size
;;;;   (set-background r g b)                  components 0.0 .. 1.0
;;;;   (set-foreground r g b)
;;;;   (set-syntax-color "keyword" r g b)      keyword function type string
;;;;                                           number comment constant variable
;;;;                                           operator punctuation default
;;;;                                           modeline modeline-inactive
;;;;                                           modeline-text
;;;;   (set-line-numbers t)                    or NIL — the editor-wide default
;;;;   (set-no-gutter-modes '("org-mode"))     the modes that overrule it, per
;;;;                                           buffer; replaces the whole list
;;;;   (set-tab-width n)
;;;;   (set-text-width n)                      columns, centred; 0 is off
;;;;   (set-modeline-relief n)                 bevel px; negative sinks it
;;;;   (set-modeline-pad n)                    padding px inside the modeline
;;;;   (set-completion-style "center")         "minibuffer" "bottom" "center"
;;;;   (clear-commands) (register-command "name")   what M-x offers
;;;;   (message text)                          status line
;;;;   (insert text)                           into the current buffer
;;;;   (find-file "path") (save-file) (save-file "path")
;;;;   (show-dashboard) (quit)
;;;;   (dashboard-banner text)
;;;;   (clear-dashboard-items)
;;;;   (dashboard-item #\f "Find file" "find-file")
;;;;   (define-key "normal" "SPC f f" "find-file")
;;;;
;;;; Readers. These answer from the live editor, so a command can depend on
;;;; where the cursor is and what is selected. All take no arguments except
;;;; `buffer-substring'; offsets are characters, counted from 0.
;;;;
;;;;   (point) (point-min) (point-max) (buffer-size)
;;;;   (line-number) (column)                  1-based line, 0-based column
;;;;   (line-count) (line-start) (line-end)
;;;;   (buffer-string) (line-string) (buffer-substring beg end)
;;;;   (buffer-name) (buffer-file-name)        the latter NIL for a scratch buffer
;;;;   (buffer-modified-p) (buffer-read-only-p) (buffer-list)
;;;;   (major-mode) (minor-modes) (evil-state) "normal" "insert" "visual" ...
;;;;   (region)                                (BEG . END), or NIL if nothing is
;;;;                                           selected
;;;;   (region-beginning) (region-end) (region-text)
;;;;   (region-ranges)                         one per line in visual block mode
;;;;   (window-scroll) (window-height) (frame-count)
;;;;
;;;; Writers, beyond `insert' above:
;;;;
;;;;   (goto-char n)
;;;;   (delete-region beg end)
;;;;   (replace-region beg end text)           atomic; see `surround-region'
;;;;   (set-evil-state "normal")
;;;;
;;;; Lisp runs on its own thread and never blocks redisplay or your typing —
;;;; unlike Emacs, where a slow function freezes the editor until it returns. The
;;;; cost is that a *sequence* of commands is not atomic: a keystroke can land
;;;; between two of them. Where that matters, use the one primitive that does the
;;;; whole job (`replace-region') rather than several that each do part of it.
;;;;
;;;; The exception is `find-file', `save-file', git and dired: those need the
;;;; application rather than the editor core, so they take effect a moment later
;;;; and a reader called immediately afterwards still sees the old buffer.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; Themes
;;;
;;; A theme is an ordinary Lisp file of `set-background', `set-foreground' and
;;; `set-syntax-color' calls, so loading one *is* applying it, and loading
;;; another afterwards switches: every theme sets every face, so nothing is left
;;; behind from the one before.
;;;
;;; Because dired and magit colour themselves out of the same faces as source
;;; code — a directory is a "type", a size is a "number" — a theme reaches every
;;; buffer without knowing that dired exists.

(defparameter *runtime-dir*
  (when *load-truename*
    (make-pathname :name nil :type nil :defaults *load-truename*))
  "The directory this config was loaded from. `*load-truename*' is only bound
during a load, so it has to be captured here rather than read later.")

(defun load-theme (name)
  "Load theme NAME from the themes/ directory next to this config."
  (let ((path (and *runtime-dir*
                   (merge-pathnames (format nil "themes/~a.lisp" name)
                                    *runtime-dir*))))
    ;; ponytail: themes are found relative to *this file*, so a config copied to
    ;; ~/.config/zemacs without the themes/ directory finds nothing and says so.
    ;; A search path is the fix when there is somewhere else to look.
    (cond ((null path) (message "load-theme: cannot tell where this config lives"))
          ((probe-file path) (load path :verbose nil :print nil)
                             (message (format nil "theme: ~a" name)))
          (t (message (format nil "no such theme: ~a" name))))))

(defun modus-vivendi () "Modus Vivendi — black ground." (load-theme "modus-vivendi"))
(defun modus-vivendi-tinted () "Modus Vivendi, tinted ground — the default."
  (load-theme "modus-vivendi-tinted"))
(defun modus-operandi () "Modus Operandi — the light one." (load-theme "modus-operandi"))

;;; ---------------------------------------------------------------------------
;;; Appearance

(defparameter *font-size* 16
  "Current point size. Kept here rather than read back from the editor, the
same way Emacs tracks `text-scale-mode-amount' in a variable.")

(set-font-size *font-size*)
(set-line-numbers t)

;;; ...and then the exceptions, buffer by buffer.
;;;
;;; `set-line-numbers' above is the editor-wide default and every buffer follows
;;; it; this names the major modes that do not. The distinction matters because
;;; the gutter is drawn per *pane*: with an org file open beside a source file,
;;; one editor-wide flag can only give them the same answer. It used to be said
;;; with `set-mode-local', and that is exactly what went wrong — a mode-local
;;; setting is global by construction, so entering org turned the numbers off in
;;; the code buffer next to it, and which pane won depended on which mode had
;;; been entered last.
;;;
;;; Prose is the case worth stating: a line number is a coordinate for talking
;;; to a compiler or another person about code, and nobody has ever cited a line
;;; of prose by number. Terminals and the dashboard are *not* here — they have no
;;; buffer lines to number and the renderer knows it, so they are not a decision
;;; anyone should have to remember to write down.
(set-no-gutter-modes '("org-mode" "org-frozen-mode" "text-mode" "tutor-mode"))
;; t counts from the cursor, vim-style — and counts *visual* lines, so with
;; wrapping on a long paragraph is numbered once per row. That is Emacs'
;; `display-line-numbers-type 'visual', and it is the reading that agrees with
;; `j' and `k': `3j' lands on the row labelled 3.
(set-relative-line-numbers t)
(set-tab-width 4)

;;; Full-width text everywhere by default: code wants every column it can get,
;;; and indentation read down the middle of a pane is indentation you have to
;;; hunt for. `org-mode' claims this for itself in `modes/library.lisp' — a
;;; measure is a fact about *prose*, not about the editor.
(set-text-width 0)

;;; What a window does with a line wider than it is.
;;; "truncate" — cut it at the pane edge and mark the tail with a `→'
;;; "wrap"     — continue it on the next row
(set-line-overflow "truncate")

;;; A calm dark theme: a near-black blue-grey ground, cool off-white text.
(set-background 0.07 0.08 0.12)
(set-foreground 0.86 0.90 1.00)

;;; Syntax faces. Any face you leave out keeps its built-in colour.
(set-syntax-color "keyword"  0.78 0.57 0.94)
(set-syntax-color "function" 0.51 0.75 1.00)
(set-syntax-color "type"     0.45 0.86 0.83)
(set-syntax-color "string"   0.62 0.85 0.55)
(set-syntax-color "number"   0.98 0.72 0.47)
(set-syntax-color "comment"  0.42 0.46 0.58)

;;; Markup faces, used by org-mode.
;;;
;;; Emphasis used to be carried by colour *instead of* weight and slant, because
;;; the renderer opened one font face. It opens several now, so `*bold*' is bold
;;; and `/italic/' is italic — and these colours changed with that, because they
;;; were doing two jobs and now do one. A tint that had to be distinguishable
;;; from body text all by itself could not also be subtle; freed of that, it can
;;; be bright enough to read as deliberate.
;;;
;;; Brighter here means *further from the background*, not further from white:
;;; the ground is a near-black blue-grey, so these are pushed up in value and
;;; kept saturated, which is what makes them sing against it rather than wash
;;; out. Three levels and no more: the highlighter paints level 3 and everything
;;; below it in the same face, so a `heading-4' here would be a line that names
;;; nothing. Depth past three is carried by the bullet and the indent.
(set-syntax-color "heading-1" 0.62 0.84 1.00) ; bright azure
(set-syntax-color "heading-2" 0.52 0.95 0.84) ; bright aqua
(set-syntax-color "heading-3" 0.86 0.78 1.00) ; bright lilac
(set-syntax-color "bold"      1.00 0.95 0.78) ; near-white gold: heaviest thing here
(set-syntax-color "italic"    0.80 0.94 0.74)
(set-syntax-color "code"      0.70 0.90 1.00)
(set-syntax-color "link"      0.55 0.82 1.00)
(set-syntax-color "markup"    0.36 0.40 0.52) ; the delimiters themselves, dim on purpose

;;; The modeline. Relief is Emacs' `:box :line-width': the magnitude is the
;;; bevel in pixels and the *sign* picks which way it goes — 2 raises the bar
;;; off the buffer, -2 sinks it into the window. 0 is flat.
(set-modeline-relief 2)
(set-modeline-pad 8)

;;; Its faces go through `set-syntax-color' like any other: the modeline lives
;;; in the same colour table, so there is nothing new to learn.
(set-syntax-color "modeline"          0.16 0.18 0.26) ; bar, current window
(set-syntax-color "modeline-inactive" 0.10 0.11 0.17) ; bar, other windows
(set-syntax-color "modeline-text"     0.80 0.85 0.97) ; what is written on it

;;; Everything above is the fallback palette. Loading a theme replaces all of
;;; it; comment this out to keep the defaults, or swap in `modus-operandi' for
;;; the light one.
(load-theme "modus-vivendi-tinted")

;;; Where completing prompts (M-x, find-file, buffer switch) are drawn.
;;; "center"     — a floating box in the middle of the window, telescope-style
;;; "bottom"     — a list growing up from the bottom edge, consult-style
;;; "minibuffer" — one plain line at the bottom, the vim prompt
(set-completion-style "center")

;;; ---------------------------------------------------------------------------
;;; Your own commands
;;;
;;; A "command" is just a zero-argument function in this package. Dashboard
;;; items and key bindings name commands as strings; anything that is not a
;;; built-in verb is called here, in the image. That is the whole extension
;;; mechanism — key bindings and dashboard items need no registration step.
;;; `M-x' is the exception: it has to know the names *before* you type them, so
;;; `refresh-commands' below publishes them.

;;; LOAD binds *LOAD-TRUENAME* while this file is being read, so the config can
;;; remember where it came from and re-read itself later.
(defvar *config-file* *load-truename*
  "Truename of the init file that was loaded at startup.")

(defun %eval-file (path)
  "LOAD PATH, republish the M-x list, and report the outcome in the status line.
*PACKAGE* is bound to ZEMACS around the LOAD so a file that never says
`(in-package :zemacs)' — the scratch buffer — can still call `message' and the
rest of the primitives unqualified."
  (handler-case
      (let ((*package* (find-package :zemacs)))
        (load path :verbose nil :print nil)
        (refresh-commands)
        (message (format nil "evaluated ~a" (file-namestring path))))
    (error (e) (message (format nil "~a: ~a" (file-namestring path) e)))))

(defun reload-config ()
  "Re-LOAD the init file, picking up edits without restarting the editor."
  (if *config-file*
      (%eval-file *config-file*)
      (message "no config file to reload")))

(defun edit-config ()
  "Open the init file for editing."
  (if *config-file*
      (find-file (namestring *config-file*))
      (message "no config file to edit")))

(defun lisp-version ()
  "Prove there is a real Common Lisp in here."
  (message (format nil "~a ~a — ~d symbol~:p in ZEMACS"
                   (lisp-implementation-type)
                   (lisp-implementation-version)
                   (let ((n 0))
                     (do-symbols (s (find-package :zemacs)) (declare (ignore s))
                       (incf n))
                     n))))

;;; Magnification, the way `text-scale-adjust' works in Emacs.

(defun set-scale (n)
  (setf *font-size* (max 6 (min 96 n)))
  (set-font-size *font-size*)
  (message (format nil "font size ~d" *font-size*)))

(defun text-scale-increase () (set-scale (+ *font-size* 2)))
(defun text-scale-decrease () (set-scale (- *font-size* 2)))
(defun text-scale-reset    () (set-scale 22))

;;; ---------------------------------------------------------------------------
;;; The scratch buffer
;;;
;;; Emacs's *scratch* has no file behind it. Ours does, because `find-file' is
;;; the only primitive that can put the editor in a *different* buffer —
;;; `insert' would drop a Lisp header into whatever you happened to be editing.
;;; A real .lisp file also gets syntax highlighting and survives a restart.

(defparameter *scratch-file*
  (merge-pathnames ".config/zemacs/scratch.lisp" (user-homedir-pathname))
  "Where the scratch buffer lives on disk.")

(defun %scratch-text ()
  "What a fresh scratch file is seeded with."
  (format nil ";;; *scratch* — ~a ~a
;;;
;;; A real Common Lisp buffer. Save it with `SPC f s', then press C-c to
;;; evaluate the file: errors, and anything you `message', land in the status
;;; line. Every symbol in the ZEMACS package is in scope unqualified.

(message (format nil \"hello from ~~a\" (lisp-implementation-type)))
"
          (lisp-implementation-type)
          (lisp-implementation-version)))

(defun lisp-scratch ()
  "Open the scratch buffer, creating it with a header the first time.
Deliberately not called `scratch': core resolves its own built-in verbs before
asking the image, so a Lisp function of that name could never be reached from a
key binding or a dashboard item."
  (handler-case
      (progn
        (ensure-directories-exist *scratch-file*)
        (unless (probe-file *scratch-file*)
          (with-open-file (out *scratch-file* :direction :output
                                              :if-does-not-exist :create
                                              :external-format :utf-8)
            (write-string (%scratch-text) out)))
        (find-file (namestring *scratch-file*)))
    (error (e) (message (format nil "scratch: ~a" e)))))

;;; ---------------------------------------------------------------------------
;;; *Messages*
;;;
;;; The log has always existed — capped at 500, readable as `(messages)' — and
;;; nothing showed it. This is the whole of showing it, and there is nothing in
;;; Rust behind it: `create-buffer' makes a buffer with no file, and unlike
;;; `find-file' it is applied on the spot, so the very next form writes into the
;;; buffer it just made rather than into the one you were leaving.
;;;
;;; Emacs' `*Messages*' is read-only and appends; this one is an ordinary buffer
;;; rewritten from the log each time you ask, which is the same thing to look at
;;; and one form to write.

(defun messages-buffer ()
  "Show the message log in a buffer, newest at the bottom."
  (let ((log (messages)))
    (create-buffer "*Messages*")
    (replace-region 0 (point-max)
                    (if log
                        (format nil "~{~a~%~}" log)
                        "no messages yet"))
    (goto-char (point-max))
    (message (format nil "~a message~:p" (length log)))))

(defun yank-buffer-file-name ()
  "Put this buffer's path in the register, and say what it copied.

The register is this editor's kill ring: `p' pastes it, and it is what every
other copy in here writes to. `set-register' takes the text and a `linewise'
flag, and a path is emphatically not a line — pasting it must land inside the
line you are on, not open a new one below it.

ponytail: the register and the system clipboard are the same thing here, so
this reaches other applications only as far as that already does. Nothing to
add until the two are separated."
  (let ((path (buffer-file-name)))
    (if path
        (progn (set-register path nil) (message path))
        (message "no file behind this buffer"))))

(defun %newest-file (&rest paths)
  "The most recently written of PATHS that exists, or NIL."
  (let ((live (remove-if-not #'probe-file (remove nil paths))))
    (first (sort live #'> :key #'file-write-date))))

(defun eval-file-dwim ()
  "Evaluate the Lisp *file* you saved most recently — the scratch buffer or the
init file — and report what happened.

Note this reads from disk, so it needs a save first. `C-c' does not use it:
that is the built-in `eval-dwim' verb, which evaluates the *live* buffer text
(the selection if there is one, else the form under point, else the whole
buffer) without touching the filesystem. This one is still handy for picking up
a config edit made in another editor."
  (let ((path (%newest-file *scratch-file* *config-file*)))
    (if path
        (%eval-file path)
        (message "nothing to evaluate: no scratch file and no config file"))))

;;; ---------------------------------------------------------------------------
;;; M-x
;;;
;;; M-x calls the name you pick as `(name)', with no arguments, so only
;;; zero-argument functions belong in the list — offering `set-scale' would just
;;; produce a wrong-number-of-arguments error.

(defparameter *lambda-list-fn* (find-symbol "FUNCTION-LAMBDA-LIST" "EXT")
  "ECL's introspection entry point, looked up rather than named literally so a
build without it still reads this file.")

;;; The host primitives are C functions: ECL has no lambda list for them and
;;; reports "unknown", so the filter below excludes all of them — including the
;;; zero-argument ones. These few are worth offering anyway.
(defparameter *extra-commands* '("quit" "show-dashboard")
  "Names published to M-x on top of what introspection finds.")

(defparameter *hidden-commands*
  (append (when (boundp '*readers*) (symbol-value '*readers*))
          '("make-marker" "point-marker" "load-theme"
            "buffer-lines" "buffer-names" "beginning-of-line" "end-of-line"
            ;; ...and one that is worse than useless by hand: called with no
            ;; argument it means "plain text", so `M-x set-language' picked by a
            ;; stray fuzzy match silently uncolours the buffer. `kill-buffer' is
            ;; deliberately *not* here — no-argument means the live buffer,
            ;; which is exactly what Emacs' `C-x k' does.
            "set-language"))
  "Zero-argument by introspection, but not things to run from M-x: they answer a
question or build a value for other code, and running one by hand does nothing
you can see. `*readers*' is the reader set the shim interns, taken wholesale so
this list does not have to be kept in step with it by hand.")

(defun %zero-arg-p (sym)
  "True when (SYM) is a legal call: no lambda list at all, or nothing but
&OPTIONAL/&REST/&KEY/&AUX parameters. Unknown arity counts as false — guessing
here would put a command in the list that errors the moment you run it."
  (let ((info (and *lambda-list-fn*
                   (ignore-errors
                    (multiple-value-list (funcall *lambda-list-fn* sym))))))
    (and (second info)                  ; second value: was it known?
         (let ((args (first info)))
           (or (null args) (member (first args) lambda-list-keywords))))))

(defun refresh-commands ()
  "Publish the zero-argument functions of this package as M-x candidates.
Clears first, so reloading the config does not duplicate the list."
  (clear-commands)
  (dolist (name *extra-commands*) (register-command name))
  (do-symbols (s (find-package :zemacs))
    (let ((name (symbol-name s)))
      (when (and (eq (symbol-package s) (find-package :zemacs)) ; not CL's
                 (fboundp s)
                 (plusp (length name))
                 (char/= (char name 0) #\%) ; internal helper
                 (not (member (string-downcase name) *hidden-commands*
                              :test #'string=))
                 (%zero-arg-p s))
        ;; Lowercase is what the user types and what the list displays; ECL
        ;; stores the name upcased.
        (register-command (string-downcase name))))))

;;; ---------------------------------------------------------------------------
;;; Dashboard
;;;
;;; The banner is plain text; the renderer centres it. Items are (key label
;;; action) and are matched by pressing the key.

;;; Built rather than pasted: the epigraph is picked per session and the version
;;; line is read out of the running image, so the screen says something true
;;; about *this* boot instead of being a picture of one.
(defparameter *koans*
  '("the listener is always listening"
    "no compile, no link, no wait"
    "(eq 'code 'data)"
    "parentheses are the shape of thought"
    "the image remembers"
    "every function is redefinable, including this one"
    "λ is not a keyword. λ is the point."
    "a REPL is a conversation, not a command")
  "One is chosen at random each boot. `format' the whole banner, not just this,
so the width stays right whichever line comes up.")

(defun %banner ()
  "The text under the logo.

Block-capital ASCII used to spell the name here, and it is gone for two
reasons. The logo above says what the application is, in artwork that does not
depend on a font having the block-drawing characters — and the art *did* depend
on that: rendered in a font missing some of them it degraded into letters that
were not the ones intended, which is a worse first impression than no artwork at
all. Letter-spaced type says the same thing in characters every font has.

No leading whitespace on any line: the dashboard centres each line itself, so
padding here would shift the block off-centre rather than move it."
  (let ((koan (nth (random (length *koans*)) *koans*)))
    (format nil "
z e m a c s

a common lisp machine that edits text

;; ~a

(~a ~a) on ~a
"
            koan
            (string-downcase (lisp-implementation-type))
            (lisp-implementation-version)
            (string-downcase (software-type)))))

(dashboard-banner (%banner))

;;; ...and a picture over it. `image-file' answers NIL when it cannot read the
;;; file, and `dashboard-logo' takes NIL to mean "no logo" — so a checkout
;;; without the assets directory falls back to the ASCII banner alone instead of
;;; leaving a hole where a lambda should be. That is the same contract
;;; `latex-preview' has, for the same reason: an asset is a thing that can be
;;; missing, and a config must survive it.
;;;
;;; Sized in ems, like every other figure, so it grows with the font rather than
;;; staying a fixed slab of pixels when the display or the point size changes.
(when *runtime-dir*
  (dashboard-logo
   (image-file (merge-pathnames "../assets/Lisp_logo.svg.png" *runtime-dir*) 10)))

(clear-dashboard-items)
;; Built-in verbs...
(dashboard-item #\f "Find file"      "find-file")
;; ...and functions defined above, on equal footing. `lisp-scratch' rather than
;; the built-in `scratch' verb, which only drops you in an empty, language-less
;; buffer nothing can evaluate.
(dashboard-item #\s "Scratch buffer" "lisp-scratch")
(dashboard-item #\e "Evaluate Lisp"  "eval-dwim")
(dashboard-item #\c "Edit configuration" "edit-config")
(dashboard-item #\r "Reload configuration" "reload-config")
(dashboard-item #\v "Lisp version" "lisp-version")
(dashboard-item #\q "Quit" "quit")

;;; ---------------------------------------------------------------------------
;;; Keys
;;;
;;; Modes: "normal" "insert" "visual" "visual-line" "visual-block" "magit"
;;; "dashboard". Sequences are space-separated tokens: SPC, C-x, <esc>, <ret>,
;;; <tab>, or a literal key. These are consulted before the built-in vim
;;; grammar, so config wins.

(defparameter *leader-modes* '("normal" "visual" "visual-line" "visual-block")
  "Modes with a SPC leader. Insert is excluded — SPC there types a space — and
so is dashboard, where single letters pick items.")

(defparameter *all-modes*
  '("normal" "insert" "visual" "visual-line" "visual-block" "dashboard" "magit")
  "Everywhere a modifier chord should work, including while typing and while a
selection is up. Listed once so a new mode cannot be quietly left out of half
the bindings.")

(defun define-key-everywhere (keys command)
  "Bind KEYS in every mode."
  (dolist (mode *all-modes*) (define-key mode keys command)))

(defun define-leader (keys command)
  "Bind a SPC-prefixed sequence in the modes that have a leader."
  (dolist (mode *leader-modes*) (define-key mode keys command)))

;;; Leader bindings work with a selection up, not just from normal mode.
(define-leader "SPC f f" "find-file")
(define-leader "SPC f s" "save-file")
(define-leader "SPC b d" "show-dashboard")
(define-leader "SPC b b" "switch-buffer")
(define-leader "SPC j j" "switch-buffer")
(define-leader "SPC h r" "reload-config")
(define-leader "SPC h v" "lisp-version")
(define-leader "SPC h m" "messages-buffer")   ; what Emacs puts on `C-h e'
(define-leader "SPC b s" "lisp-scratch")
(define-leader "SPC q q" "quit")
(define-key-everywhere "C-M-j" "switch-buffer")

;;; `M-o' jumps between windows, ace-window style: with two it just switches,
;;; with more it labels each pane and waits for you to press a label.
;;; `C-s' is consult-line — pick a line by fuzzy match, with the buffer
;;; previewing as you narrow, and Esc putting the cursor back.
(define-key-everywhere "M-o" "ace-window")
(define-key-everywhere "C-s" "search-line")
;;; `C-g' is consult-ripgrep, not quit — Esc is what aborts here. Candidates
;;; come from `rg' itself, so the pattern is a real regex and the fuzzy filter
;;; stays out of the way rather than second-guessing it.
(define-key-everywhere "C-g" "search-project")
(define-leader "SPC s l" "search-line")
(define-leader "SPC s p" "search-project")

;;; Projects. The root is found from the *current buffer* — the file on screen
;;; is the only honest answer to "which project" when two are open at once —
;;; by walking up for a `.git', `Cargo.toml', `package.json' and the like. A
;;; VCS root beats a build file, so a workspace member resolves to the repo.
;;;
;;; `SPC p p' switches: the candidates are projects visited before, and picking
;;; one opens it as a directory, which is dired. Finding a file and switching
;;; project are the same prompt because opening a root *is* switching to it.
(define-leader "SPC p f" "project-find-file")
(define-leader "SPC p p" "project-switch")
;;; `SPC p o' is the way out of the remembered list: it prompts for a path,
;;; starting at `~/', and completes a directory at a time as you type — so a
;;; project you have never opened is reachable without having opened it. What
;;; you pick opens in dired and joins the `SPC p p' list. `SPC p D' is the same
;;; gesture inside the current project.
(define-leader "SPC p o" "project-open")
(define-leader "SPC p D" "project-find-dir")
(define-leader "SPC p d" "project-dired")
(define-leader "SPC p c" "project-compile")   ; cargo build, npm run build, make
(define-leader "SPC p t" "project-test")
(define-leader "SPC p r" "project-root")      ; echo it, with what identified it
(define-leader "SPC p g" "project-forget")    ; re-walk after creating files
(define-key-everywhere "C-M-p" "project-find-file")
(define-leader "SPC w w" "ace-window")

;;; The terminal. A real shell on a real PTY, in a buffer.
;;;
;;; In `terminal' mode the shell owns the keyboard: `d', `j', Esc and above all
;;; `C-c' all reach the child, because a `C-c' that stopped at the editor would
;;; mean never being able to interrupt anything. That is why this is the one
;;; mode whose keymap is consulted *instead of* the Evil grammar rather than
;;; before it — and why only bindings made here, in "terminal", are live.
;;;
;;; `C-M-t' is the way out, into Normal mode on the same buffer, where the
;;; motions work and the scrollback can be read. The mouse wheel scrolls the
;;; history either way.
(define-leader "SPC o t" "terminal")
(define-key "terminal" "C-M-t" "terminal-normal")
;;; ...and back in, the way `i' enters Insert mode from Normal.
(define-key "normal" "C-M-t" "terminal")

;;; Clickable links in the terminal. A click the child did not ask for used to
;;; do nothing at all — a shell never turns mouse reporting on — so the row it
;;; landed on comes here instead. What counts as a link is decided in Lisp
;;; rather than in Rust, because it is policy and policy is what this file is.
(defparameter *browse-url-program*
  #+darwin "open" #+(or linux freebsd) "xdg-open" #-(or darwin linux freebsd) nil
  "The program handed a URL, or NIL to refuse.

Not a browser name: the point of `open' and `xdg-open' is that the *desktop*
decides, so an `https:' reaches the browser you actually use, a `file:' reaches
whatever opens that kind of file, and a `mailto:' reaches your mail client.")

(defun browse-url (url)
  "Hand URL to the desktop.

`:wait nil' because nothing here wants the browser's exit status and waiting for
one would park the Lisp thread on a program you are still reading. The URL is
echoed either way, so a machine with no opener still leaves you something to
copy."
  (if *browse-url-program*
      (progn (ignore-errors
              (ext:run-program *browse-url-program* (list url)
                               :wait nil :input nil :output nil :error nil))
             (message (format nil "opened ~a" url)))
      (message (format nil "no opener for ~a" url))))

(defparameter *url-schemes* '("http://" "https://" "file://" "mailto:")
  "Prefixes that make a run of text worth clicking. The policy hook: add one and
that scheme becomes clickable too.")

(defparameter *url-breaks* '(#\Space #\Tab #\" #\' #\< #\> #\( #\) #\[ #\])
  "Characters a URL cannot contain, so one printed inside quotes or brackets
still ends where the eye says it does.")

(defun %url-at (line col)
  "The URL in LINE that column COL falls inside, or NIL.

A click on the space *after* a link is a click on nothing: without that check
the run scanned backwards from a delimiter is the link, and half the blank right
half of a terminal row would open the last URL on the line."
  (let ((n (length line)))
    (when (and (< -1 col n) (not (member (char line col) *url-breaks*)))
      (flet ((break-p (c) (member c *url-breaks*)))
        (let* ((beg (1+ (or (position-if #'break-p line :end col :from-end t) -1)))
               (end (or (position-if #'break-p line :start col) n))
               ;; Trailing punctuation belongs to the sentence, not to the URL.
               ;; A link at the end of a log line is followed by a period often
               ;; enough that keeping it would break every one of them.
               (url (string-right-trim ".,;:!?" (subseq line beg end))))
          (when (some (lambda (s) (and (<= (length s) (length url))
                                       (string-equal s url :end2 (length s))))
                      *url-schemes*)
            url))))))

(defun %terminal-click (line col &optional uri)
  "A click the child did not want, on the screen row LINE at column COL.

URI is the OSC 8 link the child hung on that cell, and it wins: `cargo' marks
its error codes and `ls --hyperlink' marks its filenames that way, so the text
you clicked is a word and the link behind it is nowhere on the screen. Only when
there is no such link does the row itself get read for one."
  (let ((url (or uri (%url-at line col))))
    (when url (browse-url url))))

;;; Dired. `SPC f d' opens the directory of the current file; in a listing,
;;; the keys are Emacs' own.
(define-leader "SPC f d" "dired")
(define-key "dired" "<ret>" "dired-enter")
(define-key "dired" "-" "dired-up")

;;; Magit's own keys, in the status buffer. `TAB' is the one that makes it a
;;; buffer rather than a list: on a section it folds, on a file it opens the
;;; diff. With a diff open, `s' and `u' act on the *hunk* under the cursor —
;;; staging part of a file is what magit is used for more than anything else.
(define-key "magit" "<tab>" "magit-toggle")
(define-key "magit" "c a" "magit-amend")
(define-key "magit" "f f" "magit-fetch")
(define-key "magit" "z z" "magit-stash")
(define-key "magit" "z p" "magit-stash-pop")
;;; A rebase in flight. Stopping on a conflict is ordinary progress, not an
;;; error: fix the files, stage them, then `r c'.
(define-key "magit" "r c" "magit-rebase-continue")
(define-key "magit" "r s" "magit-rebase-skip")
(define-key "magit" "r a" "magit-rebase-abort")   ; throws the rebase away

(define-key "dired" "^" "dired-up")
(define-key "dired" "m" "dired-mark")
(define-key "dired" "u" "dired-unmark")
(define-key "dired" "t" "dired-toggle-marks")
(define-key "dired" "d" "dired-flag-delete")
(define-key "dired" "x" "dired-execute")
(define-key "dired" "R" "dired-rename")
(define-key "dired" "C" "dired-copy")
(define-key "dired" "+" "dired-mkdir")
(define-key "dired" "H" "dired-toggle-hidden")
;;; `g r', not a bare `g': a single-key binding here would claim the `g' that
;;; starts `gg', and the second one would only refresh again — so the motion
;;; every other buffer has would be the one thing a listing could not do. `g r'
;;; is what evil-collection binds refresh to for the same reason, and `g' stays
;;; a prefix, so `gg' falls through to the grammar underneath.
(define-key "dired" "g r" "dired-refresh")
(define-key "dired" "q" "show-dashboard")

;;; ---------------------------------------------------------------------------
;;; Major and minor modes
;;;
;;; A buffer has exactly one major mode, taken from its file (`notes.org' opens
;;; in `org-mode'), and any number of minor modes on top. `M-x org-mode' sets it
;;; by hand. Bindings made for a mode name that is not an editing mode belong to
;;; that major/minor mode and apply only in its buffers — minor modes are
;;; consulted first, most recently enabled first.
;;;
;;; A function named `<mode>-hook' runs whenever the mode is entered. That is
;;; the only hook the editor itself fires; everything else below — mode-local
;;; settings that revert, exit hooks, inheritance, minor modes — is built on top
;;; of it in Lisp, which is where mode machinery belongs in a Lisp machine.
;;;
;;;   (define-derived-mode NAME PARENT &body BODY)
;;;   (define-minor-mode NAME DOC (:on ...) (:off ...))
;;;   (set-mode-local MODE SETTING VALUE)   reverts when the mode is left
;;;   (define-mode-key MODE KEYS COMMAND)   inherited by derived modes
;;;   (add-auto-mode SUFFIX MODE)           pick a mode from the file name
;;;   (derived-mode-p MODE &optional OF) (minor-mode-p MODE)
;;;
;;; Loaded before any mode hook is *defined*, because `define-derived-mode'
;;; generates `<mode>-hook' — a hand-written one after this point would replace
;;; the generated one and quietly detach the machinery for that mode.

(when *runtime-dir*
  (load (merge-pathnames "modes/modes.lisp" *runtime-dir*)
        :verbose nil :print nil))

;;; Commands that read the editor rather than only configuring it.
;;;
;;; `region' answers a (BEG . END) of character offsets, or NIL when nothing is
;;; selected; `region-text' is the text between them. See the reader list at the
;;; top of this file for the rest — `point', `line-string', `buffer-name',
;;; `evil-state' and friends all work the same way.
;;;
;;; `replace-region' does the delete and the insert as *one* operation. Doing it
;;; as `delete-region' then `insert' would also work, but Lisp here runs
;;; alongside your typing rather than freezing the editor the way Emacs does, so
;;; a keystroke can land between two separate commands. One call cannot be
;;; interrupted; two can.

(defun surround-region (left right)
  "Wrap the selection in LEFT and RIGHT."
  (let ((r (region)))
    (if r
        (replace-region (car r) (cdr r)
                        (concatenate 'string left (region-text) right))
        (message "no selection"))))

(defun org-bold () (surround-region "*" "*"))
(defun org-italic () (surround-region "/" "/"))
(defun org-code () (surround-region "~" "~"))

;;; Only in org buffers, and only with something selected.
(define-key "org-mode" "SPC m b" "org-bold")
(define-key "org-mode" "SPC m i" "org-italic")
(define-key "org-mode" "SPC m c" "org-code")

;;; ---------------------------------------------------------------------------
;;; org-latex-preview — begin overlay block
;;;
;;; Written here, in Lisp, and that is the point. Rust contributes exactly two
;;; things it alone can do: `latex-fragments' scans the buffer for `$...$',
;;; `\[...\]' and `\begin{env}...\end{env}', and `latex-preview' runs one
;;; fragment through latex -> DVI -> dvipng and answers an image handle. The
;;; policy — which fragments, what to do with the old ones, what to say
;;; afterwards — is all below, where you can change it.
;;;
;;; An overlay is a range that moves with the text plus a property list, and the
;;; properties the renderer draws are `face', `background', `display' and
;;; `image'. Anything else you put on one stays in this image and can be any
;;; Lisp object at all, which is what `:latex' is being used for here: a mark
;;; saying "this one is mine", so re-previewing replaces its own overlays and
;;; leaves anybody else's alone.
;;;
;;; A cold render is a few hundred milliseconds *per fragment* and it happens on
;;; the Lisp thread — so the editor keeps drawing and keeps taking your
;;; keystrokes while a screenful of equations is typeset, and only the image
;;; queues behind it. Warm, from the on-disk cache, the whole buffer is
;;; instant.

(defun org-latex-previews (beg end)
  "Handles of the preview overlays this file made, overlapping BEG..END."
  (remove-if-not (lambda (o) (overlay-get o :latex))
                 (mapcar #'first (overlays-in beg end))))

(defun org-latex-preview-clear ()
  "Take the previews off, showing the LaTeX source again."
  (let ((ovs (org-latex-previews (point-min) (point-max))))
    (mapc #'delete-overlay ovs)
    (message (format nil "~d preview~:p cleared" (length ovs)))))

(defvar *org-latex-auto* t
  "Whether org buffers typeset their fragments by themselves.

Turned off by a pass that could not render anything — a machine with no `latex'
should say so once, not once per equation — and back on by `org-latex-preview'
succeeding, since asking by hand is how you say you have fixed it.")

(defun %org-latex-draw (fbeg fend)
  "Typeset the fragment between FBEG and FEND and hang an overlay on it. T when
LaTeX produced an image, NIL when it could not — which is the answer every
caller branches on, because it is the difference between `this equation is
wrong' and `this machine has no latex'."
  (let ((image (latex-preview (buffer-substring fbeg fend))))
    (when image
      (let ((ov (make-overlay fbeg fend)))
        (when ov
          (overlay-put ov :latex t)
          (overlay-put ov 'image image)))
      t)))

(defun %org-latex-render (beg end)
  "Preview every fragment between BEG and END, answering how many were drawn, or
NIL when one of them could not be rendered at all. Stops at the first failure:
a hundred identical `latex: not found' messages tell you nothing the first did.

Back to front: an overlay adjusts itself across an edit, but nothing here edits,
and walking backwards keeps the *offsets* from `latex-fragments' valid however
long the rendering takes."
  (let ((done 0))
    (dolist (f (reverse (latex-fragments)) done)
      (destructuring-bind (fbeg fend display) f
        (declare (ignore display))
        (when (and (< fbeg end) (> fend beg))
          (if (%org-latex-draw fbeg fend)
              (incf done)
              ;; NIL out of the DOLIST, which is this function's value.
              (return nil)))))))

(defun %org-latex-fragment-at-point ()
  "(BEG . END) of the fragment point is inside, or NIL.

`latex-fragments' scans the whole buffer and there is no reader for \"the one
here\", but the list it answers is short and already in order — so finding point
in it is a walk over a handful of pairs rather than a second pass over the text,
and no new primitive.

Both delimiters count as inside. Point on the closing `$' of `$x^2$' is in that
equation to anyone who just typed it, and a rule that said otherwise would make
`C-c r' silently do the whole buffer from the one position you are most likely
to press it from."
  (let ((p (point)))
    (dolist (f (latex-fragments))
      (destructuring-bind (fbeg fend display) f
        (declare (ignore display))
        (when (and (<= fbeg p) (<= p fend))
          (return (cons fbeg fend)))))))

(defun org-latex-preview ()
  "Show LaTeX fragments as images: the selection's, the one point is inside, or
— failing both — the whole buffer's.

The middle case is the one that makes this a command you press rather than one
you schedule. Inside `$...$' or a `\\begin{...}' block, `C-c r' renders *that*
equation: a few hundred milliseconds, against a few hundred per fragment for a
file full of them. It is also what you mean by pressing it there — you are
looking at one equation, and the buffer is not what you were asking about.

Fragments already previewed are re-done, so this doubles as `refresh' at
whichever of the three scopes it picked."
  (let* ((r (or (region) (%org-latex-fragment-at-point)))
         (beg (if r (car r) (point-min)))
         (end (if r (cdr r) (point-max))))
    (mapc #'delete-overlay (org-latex-previews beg end))
    (let ((done (%org-latex-render beg end)))
      ;; Asking by hand also *re-arms* the automatic pass: the usual reason a
      ;; machine had no `latex' is that it has one now.
      (setf *org-latex-auto* (and done t))
      (message (if done
                   (format nil "~d fragment~:p previewed" done)
                   "latex: nothing previewed — automatic previews off")))))

;;; ---------------------------------------------------------------------------
;;; ...and previewing without being asked
;;;
;;; The command above is the whole mechanism; this is the policy that decides
;;; when to run it, and the policy is entirely about *cost*. A cold render is a
;;; few hundred milliseconds per fragment and `latex-fragments' is a pass over
;;; the buffer, so the one thing this must never do is either of them per
;;; keystroke.
;;;
;;; Two triggers, and between them they are what "first-class inline LaTeX"
;;; means in practice:
;;;
;;;   entering org-mode   — the buffer arrives already typeset. Cold this costs
;;;                         one render per fragment on the Lisp thread while the
;;;                         editor keeps taking your keystrokes; warm, from the
;;;                         on-disk cache, it is instant.
;;;   leaving the line    — you finish editing `$\alpha$', move off the line,
;;;   you were editing      and it becomes an image. Which is exactly when you
;;;                         want it: rendering *while* you type would spend a
;;;                         latex run on `$\alph', `$\alpha', `$\alpha$' in turn
;;;                         and flicker an image in and out under the cursor.
;;;
;;; The line test is what makes the second one affordable. `after-change-hook'
;;; only records that something changed and which line it was — two variables,
;;; no scan — and `point-moved-hook' does the work only once the two disagree.
;;; Typing therefore costs a comparison per keystroke, and the buffer pass
;;; happens once per line you edit rather than once per character.

(defvar *org-latex-edited-line* nil
  "The line an edit last touched, or NIL when nothing is waiting to be typeset.
NIL is also the whole of the dirty flag: there is no second variable.")

(defun %org-latex-previewed-ranges ()
  "(BEG . END) of every preview overlay in the buffer, in one query.

`overlays-in' already answers (ID BEG END), so asking once for the whole buffer
and matching in the image costs one round trip; asking per fragment — which is
what `org-latex-previews' does — would cost one per equation on a hook."
  (let ((out nil))
    (dolist (o (overlays-in (point-min) (point-max)) (nreverse out))
      (when (overlay-get (first o) :latex)
        (push (cons (second o) (third o)) out)))))

(defun org-latex-preview-new ()
  "Typeset the fragments that have no preview yet, quietly.

Two queries for the whole pass — the fragments and the overlays — and then a
render only for what is genuinely new. That is what makes this cheap enough to
hang off a hook: a buffer whose equations are all drawn already costs those two
and no LaTeX at all.

Quietly matters too: a `3 fragments previewed' in the status line every time you
leave a line would be the editor talking over you. Only a *failure* is worth a
message, and only the once."
  (when (and *org-latex-auto* (derived-mode-p 'org-mode))
    (let ((have (%org-latex-previewed-ranges))
          (at (point)))
      (dolist (f (reverse (latex-fragments)))
        (destructuring-bind (fbeg fend display) f
          (declare (ignore display))
          (when (and
                 ;; Already drawn. Overlap and not containment: an overlay
                 ;; shifts with the text around it, so a fragment whose source
                 ;; has grown by a character is still the same equation.
                 (notany (lambda (r) (and (< (car r) fend) (> (cdr r) fbeg))) have)
                 ;; ...and not the one point is inside: you are still typing in
                 ;; it, and `$\alph' is a LaTeX error, not an equation.
                 (not (and (<= fbeg at) (<= at fend))))
            (unless (%org-latex-draw fbeg fend)
              (setf *org-latex-auto* nil)
              (message "latex: automatic previews off — `SPC m l' to retry")
              (return)))))))
  nil)

(defun org-latex-note-change ()
  "Remember that this line now wants typesetting. On `after-change-hook', so it
must stay this cheap: one reader and one SETF, no scan."
  (when (derived-mode-p 'org-mode)
    (setf *org-latex-edited-line* (line-number))))

(defun org-latex-maybe-preview ()
  "Typeset the edited line's fragments once point has left it.

On `point-moved-hook'. The guard is two integers, so navigating a buffer nobody
has edited costs one comparison per keystroke and nothing else."
  (when (and *org-latex-edited-line*
             (/= *org-latex-edited-line* (line-number)))
    (setf *org-latex-edited-line* nil)
    (org-latex-preview-new))
  nil)

;;; DEFVAR before each PUSHNEW, exactly as `org-modern.lisp' does it: this file
;;; is read before `lsp.lisp' installs `after-change-hook' and before
;;; `modes.lisp' would have been reached in a build with no `*runtime-dir*', so
;;; both lists have to be safe to be the first to mention.
(defvar *after-change-functions* nil)
(defvar *point-moved-functions* nil)
(pushnew 'org-latex-note-change *after-change-functions*)
(pushnew 'org-latex-maybe-preview *point-moved-functions*)

;;; `C-c r', which is what the TODO asked for and what Emacs muscle memory
;;; wants. It could not work when this was written: `C-c' is bound whole, to
;;; `eval-dwim', in every mode, and an exact match used to fire before the
;;; keymap looked for a longer one. `lisp-mode' needed the same thing for its
;;; `C-c C-e' family, so `normal_key' now lets a mode-local *prefix* outrank a
;;; global exact binding — which is this binding's whole requirement. `C-c'
;;; still evaluates everywhere else, including in org buffers on its own.
;;;
;;; `SPC m l' stays, beside the other three org commands, and
;;; `M-x org-latex-preview' works from anywhere: all of these are ordinary
;;; zero-argument functions.
(define-key "org-mode" "C-c r" "org-latex-preview")
(define-key "org-mode" "C-c R" "org-latex-preview-clear")
(define-key "org-mode" "SPC m l" "org-latex-preview")
(define-key "org-mode" "SPC m L" "org-latex-preview-clear")
;;; --- end overlay block ---

;;; `fundamental-mode', `org-mode' and `rust-mode' used to be hand-written hooks
;;; here, each having to undo what the others set. `modes.lisp' declares the
;;; same settings with `set-mode-local', which reverts them on the way out, so
;;; no mode has to know about any other.

;;; ---------------------------------------------------------------------------
;;; Magit
;;;
;;; `magit-*' are built-in verbs, run by the editor rather than by this image.
;;; The status buffer has its own mode, which is what lets `s', `u' and `c' mean
;;; stage, unstage and commit there while still meaning substitute, undo and
;;; change everywhere else — a binding is consulted before the built-in grammar,
;;; so the motions (j k gg G /) keep working in the status buffer too.

(define-leader "SPC g g" "magit-status")
(define-leader "SPC g s" "magit-status")

(define-key "magit" "s" "magit-stage")
(define-key "magit" "u" "magit-unstage")
(define-key "magit" "S" "magit-stage-all")
(define-key "magit" "U" "magit-unstage-all")
(define-key "magit" "c" "magit-commit")
(define-key "magit" "P" "magit-push")
(define-key "magit" "F" "magit-pull")
(define-key "magit" "g r" "magit-refresh")   ; `g' stays a prefix, so `gg' works
(define-key "magit" "q" "show-dashboard")

;;; C-c stays one binding — `eval-dwim' — and finishes the commit when the
;;; buffer is a commit message. Binding C-c to `magit-commit-finish' outright
;;; would take it away from every other buffer, and giving the message buffer
;;; its own mode would lose the binding the moment you pressed `i' to type.

;;; C-c evaluates Lisp, from anywhere. `eval-dwim' is a built-in verb resolved
;;; by the editor, not a function in this file: it evaluates the live buffer —
;;; the selection if there is one, else the top-level form under point, else the
;;; whole buffer — so nothing needs saving first.
;;;
;;; `C-c C-c' and not a bare `C-c', so that `C-c' is a *prefix* — which is what
;;; it is in Emacs, and what the whole `C-c &lt;letter&gt;' family below depends on.
;;;
;;; This is a rule about core, not a preference. `normal_key' looks for an exact
;;; global binding *before* it asks whether the sequence is a prefix, so while
;;; `C-c' was bound whole no global `C-c d' could ever be typed: the first key
;;; fired and the second landed in a fresh sequence. Only a *mode-local* prefix
;;; outranked an exact global one, which is why `C-c C-e' worked in a Lisp
;;; buffer and nowhere else.
;;;
;;; Nothing is lost by the move. `C-c C-c' is Emacs' own spelling for "do the
;;; thing this buffer is for", it is already what `lisp-mode' binds to
;;; `lisp-eval-defun', and it is what finishes a commit message — `eval-dwim'
;;; dispatches on the buffer kind, so one binding still covers both.
(define-key-everywhere "C-c C-c" "eval-dwim")

;;; ...except while typing, where it stays a single key. `insert_key' consults
;;; only *single-key* bindings and only in the `insert' keymap — it never waits
;;; for a second — so `C-c C-c' is unreachable there and a prefix would silently
;;; do nothing. Keeping the one-key binding in Insert also keeps the property
;;; the paragraph this replaced was describing: `C-c' evaluates rather than
;;; leaving Insert mode, and `&lt;esc&gt;' and `C-g' are still how you leave.
(define-key "insert" "C-c" "eval-dwim")

;;; The `C-c' family. These are the bindings a hand reaches for without
;;; deciding to, so they get the shortest thing that is not already spoken for.
;;;
;;; Every one of these already had a leader spelling — `SPC p p', `SPC a a' —
;;; and keeps it. A leader sequence is discoverable: hold `SPC' and which-key
;;; shows you the family. A chord is *fast*, and the two are worth having for
;;; different reasons, so this adds rather than replaces.
(define-key-everywhere "C-c d" "dired")          ; this file's directory
(define-key-everywhere "C-c t" "terminal")
(define-key-everywhere "C-c m" "magit-status")
(define-key-everywhere "C-c p" "project-switch")
(define-key-everywhere "C-c c" "project-find-file")
(define-key-everywhere "C-c a" "ai")
(define-key-everywhere "C-c i" "edit-config")
(define-key-everywhere "C-c s" "switch-buffer")
(define-key-everywhere "C-c b" "messages-buffer")
(define-key-everywhere "C-c y" "yank-buffer-file-name")
(define-key-everywhere "C-q" "delete-window")

;;; `execute-command' and `switch-buffer' are built-in verbs — core opens the
;;; prompt itself, so these names are not Lisp functions and are not in the M-x
;;; list. `SPC ;' is the usual leader spelling for M-x.
;;; "dashboard" is in this list on purpose: it is the mode the editor *opens*
;;; in, so leaving it out means M-x does nothing until you have already entered
;;; a buffer — which reads as M-x being broken.
(define-key-everywhere "M-x" "execute-command")
(define-leader "SPC ;" "execute-command")
(define-key "dashboard" "f" "find-file")
(define-key "dashboard" "b" "switch-buffer")

;;; Magnify the buffer. Meta is Command (⌘) first, with Option as a fallback, so
;;; `M-+' is ⌘-Shift-= ; `M-=' is the same key without the Shift, and works on
;;; any keyboard layout. Bound in Insert mode too, so zooming does not require
;;; leaving what you were typing.
(define-key-everywhere "M-+" "text-scale-increase")
(define-key-everywhere "M-=" "text-scale-increase")
(define-key-everywhere "M--" "text-scale-decrease")
(define-key-everywhere "M-0" "text-scale-reset")

;;; ---------------------------------------------------------------------------
;;; Scenes — a page in pixels rather than a grid of cells
;;;
;;; `gui.lisp' is the Lisp face of `crates/gui': `block', `text', `run', `image'
;;; and `rect' build a page, `scene-set' installs it on the live buffer, and a
;;; `:tag' makes a node clickable. Rust lays it out, wraps its text in a real
;;; font and routes a click back; *what* is on the page is entirely here.
;;;
;;; Loaded before the modes below because a mode that renders a document — the
;;; first will be `org-frozen-mode' — is a builder written on top of it, and
;;; before nothing else: it needs only `%do' and the face table, so it can sit
;;; anywhere above its first caller.
;;;
;;; It takes the name `block' back from Common Lisp, which is the one thing in
;;; this config that shadows a standard symbol. The file says why; the short
;;; version is that a page is written as `(block :pad 48 ...)' and `cl:block' is
;;; still there for anyone who wanted the special operator.

(when *runtime-dir*
  (handler-case
      (load (merge-pathnames "gui.lisp" *runtime-dir*) :verbose nil :print nil)
    (error (e) (message (format nil "gui: not loaded — ~a" e)))))

;;; ---------------------------------------------------------------------------
;;; Language servers — the eglot equivalent
;;;
;;; `rpc.lisp' is JSON-RPC over a child's stdin and stdout, and knows nothing
;;; about language servers. `lsp.lisp' is the whole client written on top of it:
;;; the handshake, document synchronisation, go-to-definition and diagnostics,
;;; all in Lisp. Rust owns the pipe, the framing and the process, and nothing
;;; else.
;;;
;;; Two servers ship — `pylsp' for Python and `clangd' for C — and a third is
;;; one line *here*, in your config, with no Rust to rebuild:
;;;
;;;   (lsp-register-server 'rust-mode "rust-analyzer")
;;;   (lsp-register-server 'go-mode "gopls")
;;;
;;; A server that is not installed reports in the status line the first time a
;;; buffer in its mode is touched, and nothing else breaks.
;;;
;;; Loaded after `modes.lisp', because the mode registry is what
;;; `lsp-register-server' names, and after the settings above, because loading
;;; it installs `after-change-hook' and there is no reason for that to fire
;;; while the config is still being read.

(when *runtime-dir*
  (handler-case
      (progn
        (load (merge-pathnames "rpc.lisp" *runtime-dir*) :verbose nil :print nil)
        (load (merge-pathnames "lsp.lisp" *runtime-dir*) :verbose nil :print nil))
    (error (e) (message (format nil "lsp: not loaded — ~a" e)))))

;;; `g d' is the vim spelling and wins over the built-in grammar, which is what
;;; a binding in this file always does. The `SPC l' family is the leader
;;; spelling for the rest.
(when (fboundp 'lsp-goto-definition)
  (define-key "normal" "g d" "lsp-goto-definition")
  (define-leader "SPC l l" "lsp")
  (define-leader "SPC l d" "lsp-goto-definition")
  (define-leader "SPC l e" "lsp-diagnostics-at-point")
  (define-leader "SPC l E" "lsp-list-diagnostics")
  (define-leader "SPC l r" "lsp-restart")
  (define-leader "SPC l q" "lsp-stop")
  (define-leader "SPC l s" "lsp-status"))

;;; ---------------------------------------------------------------------------
;;; which-key, Common Lisp editing, a REPL, and org's markup drawn rather than
;;; typed
;;;
;;; Five files, loaded in order because each uses the one before it:
;;;
;;;   which-key.lisp  what continues the prefix you just pressed, in the status
;;;                   line — and the same table read the other way round, as the
;;;                   docstring and key `M-x' now shows beside a command.
;;;   lisp-mode.lisp  one scanner for the shape of Lisp text, and the motion,
;;;                   kill, slurp/barf and indentation commands built on it.
;;;   repl.lisp       `C-c C-e' and friends, evaluating in *this* image and
;;;                   writing form and value into a transcript buffer.
;;;   parinfer.lisp   the inverse of that indenter — the indentation says where
;;;                   the closing parentheses go — on the same scanner.
;;;   org-modern.lisp `display' overlays: heading stars become bullets, `[X]'
;;;                   becomes a tick, and `*bold*' shows its asterisks only
;;;                   while the cursor is in it. Loaded after `lsp.lisp' so it
;;;                   finds the `after-change-hook' that file installs.
;;; term-agent:
;;;   ai.lisp         coding agents — Claude Code, Cursor, opencode — as
;;;                   ordinary buffers, on the terminal the editor already has.
;;;                   `C-a' is the menu. The harness list is *data* in that
;;;                   file, so a fourth tool is one line and no Rust; the resume
;;;                   flags are each tool's own. It loads last because it uses
;;;                   `define-leader' and pushes onto `*extra-commands*', which
;;;                   `refresh-commands' below then publishes.
;;;
;;;   org-fold.lisp   code folding's policy half: what an org subtree *is*, and
;;;                   the `z a' / `z M' / `z R' commands over the one thing Rust
;;;                   owns — an overlay carrying `fold' makes the lines after
;;;                   its first stop occupying rows. `*fold-subtree-functions*'
;;;                   is where another mode joins in.
;;;
;;;   org-frozen.lisp org as a *printed page*: `org-frozen-mode', which derives
;;;                   from `org-mode' and is read-only for real. Drawers,
;;;                   `#+keyword:' lines and block delimiters stop occupying
;;;                   rows; `#+TITLE:' is typeset as a title; a `#+begin_src'
;;;                   body gets a band, a gutter and *its own language's*
;;;                   highlighting; a table is drawn with aligned columns under a
;;;                   rule. `SPC m z' toggles it either way. Loaded after
;;;                   `org-fold.lisp' because it rebinds TAB over that file's
;;;                   `org-cycle', and before `math.lisp' because a curriculum is
;;;                   what it was built to display.
;;;
;;;   tutor.lisp      `SPC h t' — the tutorial, as a buffer that marks your
;;;                   homework rather than a page of prose that trusts you.
;;;                   Stage 1 teaches Common Lisp and checks each answer in a
;;;                   *child* `ecl' with a timeout, so a student's `(loop)'
;;;                   costs them the exercise and not the image; Stage 2
;;;                   teaches the API in this file and checks by watching the
;;;                   live editor. Last in the list because it uses
;;;                   `%eval-source' from `repl.lisp' and `executable-find'
;;;                   from `ai.lisp'.
;;;
;;;   math.lisp       a whole maths curriculum as one org file — units,
;;;                   problems, and a place for your answer, all of it ordinary
;;;                   org with `#+ZEMACS_*' properties on the headings. The
;;;                   format is specified in `docs/curriculum.org', precisely
;;;                   enough to hand to a model as "generate one of these".
;;;                   Loaded after `org-modern.lisp' because it reads that
;;;                   file's org helpers and hangs itself off the
;;;                   `*org-mode-functions*' hook declared there.
;;;
;;;   math-code.lisp  the other half of a `programming' problem: its
;;;                   `#+begin_src python' block tangled to a file beside the
;;;                   curriculum, a venv built for it in the background, a
;;;                   window beside the question and one key that runs it in a
;;;                   terminal. Last of all, because it reads the schema from
;;;                   `math.lisp' and `executable-find' from `ai.lisp'.
;;;
;;;   math-written.lisp
;;;                   the other half of a `written' problem: a photograph of your
;;;                   handwriting, dropped in `~/Public/MathSync' by a phone,
;;;                   transcribed into org with LaTeX by a vision model and
;;;                   written into the Response of the problem *point is in*.
;;;                   The watcher is an ECL thread of its own, so it fires with
;;;                   nobody at the keyboard; everything slow happens on it, and
;;;                   nothing at all is deleted. `SPC m r' does one by hand.
;;;                   Beside `math-code.lisp' and for the same reasons.
;;;
;;; Loaded here rather than next to `modes.lisp' because they use `define-leader'
;;; and `define-mode-key', which are defined above this point and not below it.
(when *runtime-dir*
  (dolist (file '("modes/which-key.lisp" "modes/lisp-mode.lisp" "modes/repl.lisp"
                  "modes/parinfer.lisp" "modes/org-modern.lisp" "modes/org-fold.lisp"
                  "modes/org-frozen.lisp"
                  "modes/math.lisp"
                  "modes/ai.lisp" "modes/tutor.lisp"
                  "modes/math-code.lisp" "modes/math-written.lisp"))
    (load (merge-pathnames file *runtime-dir*) :verbose nil :print nil)))

;;; ---------------------------------------------------------------------------

;;; Last, so that every function defined above is in the list.
(refresh-commands)

(message (format nil "zemacs: init.lisp loaded — ~a ~a is driving the editor."
                 (lisp-implementation-type)
                 (lisp-implementation-version)))
