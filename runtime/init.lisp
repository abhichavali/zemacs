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
;;;; Host primitives live in the ZEMACS package. Each one sends a command to the
;;;; editor thread; none of them read editor state back.
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

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; Appearance

(defparameter *font-size* 22
  "Current point size. Kept here rather than read back from the editor, the
same way Emacs tracks `text-scale-mode-amount' in a variable.")

(set-font-size *font-size*)
(set-line-numbers t)
(set-relative-line-numbers nil)   ; t counts from the cursor, vim-style
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
                 (%zero-arg-p s))
        ;; Lowercase is what the user types and what the list displays; ECL
        ;; stores the name upcased.
        (register-command (string-downcase name))))))

;;; ---------------------------------------------------------------------------
;;; Dashboard
;;;
;;; The banner is plain text; the renderer centres it. Items are (key label
;;; action) and are matched by pressing the key.

(dashboard-banner "
 ███████╗███████╗███╗   ███╗ █████╗  ██████╗███████╗
 ╚══███╔╝██╔════╝████╗ ████║██╔══██╗██╔════╝██╔════╝
   ███╔╝ █████╗  ██╔████╔██║███████║██║     ███████╗
  ███╔╝  ██╔══╝  ██║╚██╔╝██║██╔══██║██║     ╚════██║
 ███████╗███████╗██║ ╚═╝ ██║██║  ██║╚██████╗███████║
 ╚══════╝╚══════╝╚═╝     ╚═╝╚═╝  ╚═╝ ╚═════╝╚══════╝

        a Common Lisp machine that edits text
")

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
(define-leader "SPC b s" "lisp-scratch")
(define-leader "SPC q q" "quit")
(define-key-everywhere "C-M-j" "switch-buffer")

;;; ---------------------------------------------------------------------------
;;; Major and minor modes
;;;
;;; A buffer has exactly one major mode, taken from its file (`notes.org' opens
;;; in `org-mode'), and any number of minor modes on top. `M-x org-mode' sets it
;;; by hand. Bindings made for a mode name that is not an editing mode belong to
;;; that major/minor mode and apply only in its buffers — minor modes are
;;; consulted first, most recently enabled first.
;;;
;;; A function named `<mode>-hook' runs whenever the mode is entered. Defining
;;; one is the whole extension point; there is nothing to register.

(defun org-mode-hook ()
  "Prose wants wrapped lines and absolute numbering."
  (set-line-overflow "wrap")
  (message "org-mode"))

(defun fundamental-mode-hook ()
  "Back to the defaults for anything without a mode of its own."
  (set-line-overflow "truncate"))

(defun rust-mode-hook ()
  (set-line-overflow "truncate"))

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

;;; Last, so that every function defined above is in the list.
(refresh-commands)

(message (format nil "zemacs: init.lisp loaded — ~a ~a is driving the editor."
                 (lisp-implementation-type)
                 (lisp-implementation-version)))
