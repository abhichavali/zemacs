;;;; zemacs default configuration — Common Lisp.
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
;;;;   (set-line-numbers t)                    or NIL
;;;;   (set-tab-width n)
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

(defparameter *font-size* 22
  "Current point size. Kept here rather than read back from the editor, the
same way Emacs tracks `text-scale-mode-amount' in a variable.")

(set-font-size *font-size*)
(set-line-numbers t)
;; t counts from the cursor, vim-style — and counts *visual* lines, so with
;; wrapping on a long paragraph is numbered once per row. That is Emacs'
;; `display-line-numbers-type 'visual', and it is the reading that agrees with
;; `j' and `k': `3j' lands on the row labelled 3.
(set-relative-line-numbers nil)
(set-tab-width 4)

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

;;; Markup faces, used by org-mode. Emphasis is carried by colour rather than
;;; by weight or slant — the renderer opens one font face, so real bold and
;;; italic would mean loading two more.
(set-syntax-color "heading-1" 0.60 0.80 1.00)
(set-syntax-color "heading-2" 0.55 0.88 0.80)
(set-syntax-color "heading-3" 0.80 0.75 0.98)
(set-syntax-color "bold"      1.00 0.92 0.72)
(set-syntax-color "italic"    0.78 0.88 0.72)
(set-syntax-color "code"      0.72 0.82 0.95)
(set-syntax-color "link"      0.52 0.76 0.98)
(set-syntax-color "markup"    0.36 0.40 0.52) ; the delimiters themselves

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
  (let ((koan (nth (random (length *koans*)) *koans*)))
    (format nil "
      ╭────────────────────────────────────────────────╮
      │  ▄▄▄▄▄▄▄▄  ▄▄▄▄▄▄▄  ▄▄▄   ▄▄▄  ▄▄▄▄▄   ▄▄▄▄▄▄  │
      │     ███    ██▄▄▄    ████ ████  ██▄▄██  ██▄▄▄   │
      │    ███     ██▀▀▀    ██ ███ ██  ██▀▀██  ██▀▀▀   │
      │  ▀▀▀▀▀▀▀▀  ▀▀▀▀▀▀▀  ▀▀  ▀  ▀▀  ▀▀  ▀▀  ▀▀▀▀▀▀  │
      ╰────────────────────────────────────────────────╯

              (a common lisp machine that edits text)

         ;; ~a
         (~a ~a) on ~a
"
            koan
            (string-downcase (lisp-implementation-type))
            (lisp-implementation-version)
            (string-downcase (software-type)))))

(dashboard-banner (%banner))

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
(define-key "dired" "g" "dired-refresh")
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

(defun org-latex-preview ()
  "Show every LaTeX fragment as an image — the selection's, or the buffer's.

Fragments already previewed are re-done, so this doubles as `refresh'."
  (let* ((r (region))
         (beg (if r (car r) (point-min)))
         (end (if r (cdr r) (point-max)))
         (done 0))
    (mapc #'delete-overlay (org-latex-previews beg end))
    ;; Back to front: an overlay adjusts itself across an edit, but nothing here
    ;; edits, and walking backwards keeps the *offsets* from `latex-fragments'
    ;; valid however long the rendering takes.
    (dolist (f (reverse (latex-fragments)))
      (destructuring-bind (fbeg fend display) f
        (declare (ignore display))
        (when (and (< fbeg end) (> fend beg))
          (let ((image (latex-preview (buffer-substring fbeg fend))))
            (when image
              (let ((ov (make-overlay fbeg fend)))
                (when ov
                  (overlay-put ov :latex t)
                  (overlay-put ov 'image image)
                  (incf done))))))))
    (message (format nil "~d fragment~:p previewed" done))))

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
(define-key "magit" "g" "magit-refresh")
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
;;; In Insert mode this *replaces* the built-in "C-c is a synonym for Esc":
;;; `insert_key' looks the key up in the user keymap before it reaches that
;;; rule, so this binding wins and C-c no
;;; longer leaves Insert mode. <esc> and C-g still do.
(define-key-everywhere "C-c" "eval-dwim")

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
;;; Loaded here rather than next to `modes.lisp' because they use `define-leader'
;;; and `define-mode-key', which are defined above this point and not below it.
(when *runtime-dir*
  (dolist (file '("modes/which-key.lisp" "modes/lisp-mode.lisp" "modes/repl.lisp"
                  "modes/parinfer.lisp" "modes/org-modern.lisp" "modes/org-fold.lisp"
                  "modes/ai.lisp"))
    (load (merge-pathnames file *runtime-dir*) :verbose nil :print nil)))

;;; ---------------------------------------------------------------------------

;;; Last, so that every function defined above is in the list.
(refresh-commands)

(message (format nil "zemacs: init.lisp loaded — ~a ~a is driving the editor."
                 (lisp-implementation-type)
                 (lisp-implementation-version)))
