;;;; Major and minor modes — the machinery.
;;;;
;;;; The editor hands us the raw ingredients and nothing more: a buffer has one
;;;; `major-mode' and a list of `minor-modes', a key bound to a mode *name*
;;;; applies only in that mode's buffers, and when a buffer's major mode becomes
;;;; X the editor calls the Lisp function `X-hook' if one is defined. That is
;;;; the entire contract. Exit hooks, inheritance, settings that revert when you
;;;; leave a mode, and choosing a mode from a filename are built here, in Lisp,
;;;; for the same reason Emacs builds them in elisp rather than in C: this is
;;;; the part that every config wants to bend, and the image is where bending
;;;; costs nothing.
;;;;
;;;; The one event the Rust side reports is "you are now in X". Everything else
;;;; below is deduced from it. `*major-mode*' remembers what we last saw, so a
;;;; hook naming a *different* mode is a mode change, and a change is what fires
;;;; `X-exit-hook' for the mode being left.
;;;;
;;;; What a config writes:
;;;;
;;;;   (define-derived-mode rust-mode prog-mode BODY...)   a major mode
;;;;   (define-minor-mode visual-line "doc" (:on ...) (:off ...))
;;;;   (set-mode-local 'org-mode 'line-overflow "wrap")    reverts on exit
;;;;   (define-mode-key "prog-mode" "SPC t w" "visual-line")  inherited
;;;;   (add-auto-mode ".md" 'text-mode)
;;;;   (derived-mode-p 'prog-mode)          tests the live buffer's mode
;;;;   (minor-mode-p 'visual-line)
;;;;   (defun org-mode-exit-hook () ...)    fires when org-mode is left
;;;;
;;;; Loading this file is the whole installation — it pulls in `library.lisp'
;;;; next to it, which defines the modes we ship. Load it EARLY in your init
;;;; file, above the appearance settings: `%wrap-setting-primitives' can only
;;;; record a baseline for a setting that is set *after* it takes over.
;;;;
;;;; A mode whose `X-hook' this file did not generate is invisible to all of
;;;; this — we are never told it was entered. That is why `library.lisp'
;;;; registers every mode the editor can produce on its own, `fundamental-mode'
;;;; included, rather than only the interesting ones.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; Names
;;;
;;; A mode is a string to the editor and a symbol to the config writer, who
;;; wants to type `'org-mode' rather than `"org-mode"'. Both spellings mean the
;;; same mode everywhere below.

(defun %mode-name (mode)
  "MODE as the string the editor uses. Symbols arrive upcased from the reader."
  (string-downcase (string mode)))

;;; ---------------------------------------------------------------------------
;;; The registry
;;;
;;; DEFPARAMETER rather than DEFVAR for all four: reloading the config re-runs
;;; every `define-derived-mode', so starting empty is what drops a mode you have
;;; since deleted. The tables that must *not* be rebuilt on a reload are further
;;; down, and say so.

(defparameter *mode-parents* (make-hash-table :test #'equal)
  "Mode name -> the mode it derives from, or NIL.")

(defparameter *mode-bodies* (make-hash-table :test #'equal)
  "Mode name -> a thunk run on entry, after its parent's.")

(defparameter *mode-locals* (make-hash-table :test #'equal)
  "Mode name -> alist of (SETTING . VALUE) claimed while the mode is active.
Major and minor modes share this table; nothing about a claim cares which it is.")

(defparameter *mode-keys* (make-hash-table :test #'equal)
  "Mode name -> list of (KEYS . COMMAND) declared with `define-mode-key', kept
so a mode defined later can inherit its parent's bindings.")

(defun %mode-chain (mode)
  "MODE and its ancestors, outermost first, so a child's claims are applied
last and win. A parent loop stops rather than hangs — a config that says a mode
derives from itself is a typo, not a reason to wedge the image."
  (let ((chain nil) (name (and mode (%mode-name mode))))
    (loop while (and name (not (member name chain :test #'string=)))
          do (push name chain)
             (setf name (gethash name *mode-parents*)))
    chain))

(defun derived-mode-p (mode &optional (of (major-mode)))
  "MODE's name when OF is MODE or descends from it, else NIL. OF defaults to
the live buffer's major mode, so `(derived-mode-p 'prog-mode)' reads exactly as
it does in Emacs, and passing OF makes it a question about a mode you name."
  (let ((mode (%mode-name mode)))
    (when (member mode (%mode-chain of) :test #'string=) mode)))

;;; ---------------------------------------------------------------------------
;;; Mode-local settings
;;;
;;; The settings below are global in the editor: there is one line-overflow, not
;;; one per buffer. So `org-mode-hook' calling `(set-line-overflow "wrap")' used
;;; to mean every *other* mode's hook had to know to set it back — which does
;;; not scale past two modes, since each new mode would have to be taught about
;;; every setting any other mode touches.
;;;
;;; Instead a mode *claims* a setting, and the effective value is recomputed
;;; from the bottom up whenever the modes change: baseline, then the major mode
;;; and its ancestors, then each active minor mode. Recomputing rather than
;;; saving and restoring in pairs is what makes it order-proof — entering a
;;; mode, leaving one and toggling a minor mode interleave freely, and a
;;; recomputation cannot get out of step with itself the way a stack of saved
;;; values can.

(defparameter *mode-settings*
  '((line-overflow         set-line-overflow         "truncate")
    (line-numbers          set-line-numbers          t)
    (relative-line-numbers set-relative-line-numbers nil)
    (tab-width             set-tab-width             4)
    (font-size             set-font-size             18))
  "The settings a mode may claim: (NAME PRIMITIVE DEFAULT). DEFAULT is what the
editor itself starts with, and is where a setting lands when a mode releases it
and nothing has ever set it globally.")

(defvar *setting-primitive* (make-hash-table)
  "SETTING -> the primitive as it was before we wrapped it. DEFVAR and not
DEFPARAMETER on purpose: a config reload reads this file again, and re-capturing
would store the wrapper as the raw function and recurse until the stack ran out.")

(defvar *setting-global* (make-hash-table)
  "SETTING -> the value in effect when no mode claims it. The baseline.")

(defvar *setting-applied* (make-hash-table)
  "SETTING -> what we last pushed at the editor. There is no reader for any of
these settings, so this table is the only record of what the editor now has, and
is what keeps a refresh from re-sending values that have not moved.")

(defun %effective-setting (setting)
  "The value SETTING should have right now."
  (let ((value (gethash setting *setting-global*)))
    (dolist (mode (append (%mode-chain *major-mode*) (minor-modes)) value)
      (let ((claim (assoc setting (gethash (%mode-name mode) *mode-locals*))))
        (when claim (setf value (cdr claim)))))))

(defun %refresh-settings ()
  "Bring every setting to what the current modes ask for."
  (dolist (entry *mode-settings*)
    (let ((setting (first entry)))
      (let ((want (%effective-setting setting)))
        (unless (equal want (gethash setting *setting-applied*))
          (setf (gethash setting *setting-applied*) want)
          (funcall (gethash setting *setting-primitive*) want))))))

(defun %wrap-setting-primitives ()
  "Take over the setting primitives, so that a plain `(set-tab-width 2)'
anywhere in a config records the *baseline* — the value a setting returns to
when the mode that claimed it is left. Nothing else can observe that: these
settings are global in the editor and none of them has a reader, so a wrapper
around the writer is the only place the information exists.

A global set made while a mode claims that setting is remembered but does not
take effect until the mode is left, which is the same shape as `setq-default'
under a buffer-local binding in Emacs.

ponytail: the baseline starts at the editor's factory default, so a setting
changed *before* this file is loaded is not seen — put the `(load ...)' line
above your appearance settings and it is. The fix, if that ever bites, is a
reader per setting on the Rust side and one call to it here."
  (dolist (entry *mode-settings*)
    (destructuring-bind (setting primitive default) entry
      (unless (gethash setting *setting-primitive*)
        (setf (gethash setting *setting-primitive*) (fdefinition primitive)
              (gethash setting *setting-global*) default
              (gethash setting *setting-applied*) default)
        ;; LET so the closure captures its own binding rather than the loop's.
        (let ((setting setting))
          (setf (fdefinition primitive)
                (lambda (value)
                  (setf (gethash setting *setting-global*) value)
                  (%refresh-settings))))))))

(%wrap-setting-primitives)

(defun set-mode-local (mode setting value)
  "Claim SETTING for MODE: while MODE is active the editor has VALUE, and
leaving MODE puts back whatever was in effect before. MODE may be a major or a
minor mode. Re-claiming replaces the previous claim, so a reload does not stack."
  (let ((name (%mode-name mode)))
    (if (assoc setting *mode-settings*)
        (progn
          (setf (gethash name *mode-locals*)
                (acons setting value
                       (remove setting (gethash name *mode-locals*) :key #'car)))
          (%refresh-settings))
        (message (format nil "no such mode setting: ~a" setting)))))

;;; ---------------------------------------------------------------------------
;;; Entering and leaving a major mode
;;;
;;; `X-hook' is the only thing the editor calls, so it is where all of this
;;; hangs from. `define-derived-mode' generates one per mode; a mode nobody
;;; defined keeps whatever hand-written `X-hook' it has and is otherwise unknown
;;; to the machinery.

(defvar *major-mode* nil
  "The major mode this image believes the live buffer is in. The editor reports
a mode being *entered* and never one being left, so this is what a mode change
is measured against. DEFVAR: a config reload must not forget where we are.")

(defun %run-hook (name)
  "Call the function NAME if there is one, exactly as the editor calls `X-hook'
— an undefined hook is silence, not an error, or every mode would need one."
  (let ((symbol (find-symbol (string-upcase name) :zemacs)))
    (when (and symbol (fboundp symbol)) (funcall symbol))))

(defun %enter-major-mode (name)
  "The body of every `X-hook' this file generates: NAME has just become the
major mode of the live buffer.

ponytail: switching *buffers* does not go through here, because the editor
reports no hook for it — so the settings on screen follow the last mode
*entered* rather than the buffer you are looking at. Two buffers in the same
mode, or opening a file, are both fine; flipping between an org buffer and a
rust one with C-M-j is not. The fix is a hook on the buffer switch, which is a
line in core, not a line here."
  (let ((previous *major-mode*))
    (unless (equal previous name)
      ;; Recorded before the exit hook runs, so a hook that switches modes
      ;; itself is not immediately undone by the assignment landing after it.
      (setf *major-mode* name)
      (when previous
        (%run-hook (concatenate 'string previous "-exit-hook")))))
  ;; Parents first, so a child's body sees what its parent set and can override
  ;; it — the same order the settings are resolved in.
  (dolist (mode (%mode-chain name))
    (let ((body (gethash mode *mode-bodies*)))
      (when body (funcall body))))
  (%refresh-settings)
  (%auto-mode-dispatch name))

;;; ---------------------------------------------------------------------------
;;; Choosing a mode from the filename
;;;
;;; The editor already picks a major mode from the file's *language*, which it
;;; knows for the eight it can highlight. This is the other half: the files it
;;; has no grammar for, which all arrive as `fundamental-mode'.

(defparameter *auto-mode-alist*
  '(("README"    . "text-mode")
    ("LICENSE"   . "text-mode")
    ("CHANGELOG" . "text-mode")
    (".txt"      . "text-mode")
    (".text"     . "text-mode")
    (".md"       . "text-mode")
    (".markdown" . "text-mode"))
  "(SUFFIX . MODE), first match wins. A suffix and not a regexp because a
suffix is what an extension *is*, and it costs nothing: a bare filename is a
suffix of its own path, which is how the three entries above match.")

(defun add-auto-mode (suffix mode)
  "Open files whose name ends in SUFFIX in MODE. Pushed on the front, so an
entry from a config beats a shipped one."
  (push (cons suffix (%mode-name mode)) *auto-mode-alist*))

(defun %suffixp (suffix string)
  "True when STRING ends in SUFFIX, ignoring case — extensions are not
case-sensitive to anyone but the filesystem."
  (let ((n (length suffix)) (m (length string)))
    (and (<= n m) (string-equal suffix string :start2 (- m n)))))

(defun auto-mode-for (path)
  "The major mode PATH should open in, or NIL. PATH may be NIL, which is what
`buffer-file-name' answers for a buffer with no file behind it."
  (when path
    (cdr (find-if (lambda (entry) (%suffixp (car entry) path)) *auto-mode-alist*))))

(defun %auto-mode-dispatch (name)
  "Consulted only from `fundamental-mode', which is precisely the case where
the editor had no language of its own to go on. Doing it from every mode would
mean `M-x text-mode' in a .rs buffer bounced straight back to `rust-mode'."
  (when (string= name "fundamental-mode")
    (let ((want (auto-mode-for (buffer-file-name))))
      ;; The guard also stops the bounce: setting the mode fires its hook, which
      ;; lands back here, and the second time round the mode already matches.
      (when (and want (not (string= want name)))
        (set-major-mode want)))))

;;; ---------------------------------------------------------------------------
;;; Defining a major mode

(defun %inherit-mode-keys (mode)
  "Re-issue every binding MODE's ancestors declared, under MODE's own name.
The editor's mode keymap is keyed by the exact mode name, so a key bound for
`prog-mode' would never be found in a `rust-mode' buffer otherwise."
  (dolist (ancestor (butlast (%mode-chain mode)))
    (dolist (binding (gethash ancestor *mode-keys*))
      (define-key mode (car binding) (cdr binding)))))

(defun define-mode-key (mode keys command)
  "Bind KEYS to COMMAND in MODE's buffers and in those of every mode derived
from it. Plain `define-key' still works and binds in MODE alone; this is the
version that is inherited."
  (let ((name (%mode-name mode)))
    (push (cons keys command) (gethash name *mode-keys*))
    (define-key name keys command)
    ;; A parent can gain a binding long after its children were defined, so the
    ;; inheritance has to run in this direction too.
    (maphash (lambda (child parent)
               (declare (ignore parent))
               (when (and (not (string= child name))
                          (member name (%mode-chain child) :test #'string=))
                 (define-key child keys command)))
             *mode-parents*)))

(defmacro define-derived-mode (name parent &body body)
  "Define major mode NAME deriving from PARENT (NIL for a root mode).

Defines two functions: NAME, the command that puts the live buffer in the mode
and which `M-x' offers, and NAME-hook, which the editor calls when the mode is
entered and which drives everything in this file. BODY runs on every entry,
after PARENT's body.

Settings and keys are declared *next to* the definition rather than inside it,
with `set-mode-local' and `define-mode-key', because both are facts about the
mode rather than work to redo each time it is entered.

Note that NAME-hook belongs to the machinery for a mode defined this way — put
what you would have put in it in BODY. NAME-exit-hook is yours to define."
  (let ((mode (%mode-name name))
        (parent-name (and parent (%mode-name parent))))
    `(progn
       (setf (gethash ,mode *mode-parents*) ,parent-name)
       (setf (gethash ,mode *mode-bodies*) (lambda () ,@body))
       (%inherit-mode-keys ,mode)
       ;; Real DEFUNs, not closures stuffed into `fdefinition': `M-x' finds its
       ;; candidates by asking ECL for each symbol's lambda list, and a closure
       ;; has none to report, so a mode installed that way would never appear.
       (defun ,(intern (string-upcase mode) :zemacs) ()
         ,(format nil "Put the live buffer in ~a." mode)
         (set-major-mode ,mode))
       (defun ,(intern (string-upcase (concatenate 'string mode "-hook")) :zemacs) ()
         (%enter-major-mode ,mode))
       ;; Registered here as well as by `refresh-commands', so the mode reaches
       ;; `M-x' whether this file is loaded before that call or after it.
       (register-command ,mode)
       ,mode)))

;;; ---------------------------------------------------------------------------
;;; Minor modes
;;;
;;; Whether one is on is not tracked here: `minor-modes' answers from the live
;;; buffer, which is both shorter and still right after a buffer switch —
;;; something nothing tells this image about.

(defparameter *minor-mode-bodies* (make-hash-table :test #'equal)
  "Mode name -> (ON-THUNK . OFF-THUNK).")

(defun minor-mode-p (mode)
  "True when MODE is on in the live buffer."
  (member (%mode-name mode) (minor-modes) :test #'string=))

(defun %toggle-minor-mode (name)
  "Flip NAME in the live buffer and run whichever of its bodies applies.

The bodies run here rather than from the `NAME-on-hook' the editor pushes: that
hook is delivered on the next turn of the *application* loop, and a minor mode
is only ever switched by this command, so running it on the spot is both
immediate and true in a headless image. The cost is that a bare
`(set-minor-mode \"visual-line\" t)' flips the mode without running its body —
call the command instead."
  (let ((on (not (minor-mode-p name))))
    (set-minor-mode name on)
    (let ((bodies (gethash name *minor-mode-bodies*)))
      (when bodies (funcall (if on (car bodies) (cdr bodies)))))
    (%refresh-settings)
    (message (format nil "~a ~:[off~;on~]" name on))))

(defmacro define-minor-mode (name doc &body clauses)
  "Define minor mode NAME, whose command toggles it in the live buffer.

DOC is the docstring. CLAUSES are optional and are `(:on FORMS...)' and
`(:off FORMS...)', run when the mode is switched on and off. Most minor modes
here need neither, because what they want to change is a setting, and a setting
is claimed declaratively with `set-mode-local'.

A minor mode gets its own keymap for free: `(define-key \"visual-line\" ...)'
binds in its buffers only, and minor modes are consulted before the major one."
  (let ((mode (%mode-name name)))
    `(progn
       (setf (gethash ,mode *minor-mode-bodies*)
             (cons (lambda () ,@(rest (assoc :on clauses)))
                   (lambda () ,@(rest (assoc :off clauses)))))
       (defun ,(intern (string-upcase mode) :zemacs) ()
         ,doc
         (%toggle-minor-mode ,mode))
       (register-command ,mode)
       ,mode)))

;;; ---------------------------------------------------------------------------
;;; The modes we ship live next door.
;;;
;;; LOAD binds *LOAD-TRUENAME* to *this* file while it is being read, so the
;;; sibling is found relative to it rather than to whatever the working
;;; directory happens to be — the init file loads this one by absolute path and
;;; never changes directory. Guarded because evaluating this buffer by hand
;;; (`C-c') is not a LOAD and leaves it NIL.

(when *load-truename*
  (load (merge-pathnames "library.lisp" *load-truename*) :verbose nil :print nil))
