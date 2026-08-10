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
;;;;   (enable-minor-mode 'org-modern)      on if off; the hook-safe switch
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
;;; Splitting a string, and running a program
;;;
;;; Not about modes, and here anyway, because several modes each need them and
;;; this is the earliest file every one of them already loads:
;;; `define-derived-mode' lives below, so a mode file that had not loaded this
;;; one could not have declared itself in the first place. `library.lisp' pulls
;;; it in at its very top — above `gui.lisp' and above every file in this
;;; directory — and the mode tests in `crates/lisp/tests/' each load it as their
;;; first line, so a helper defined here is in scope in both places without
;;; anything having to be arranged.
;;;
;;; `utf8-text' used to head this section, for exactly that reason: buffer text
;;; arrived as UTF-8 *bytes* and every mode had to decode it before handing any
;;; of it back. It is gone — `f_query' in `crates/lisp/src/shim.c' decodes now,
;;; once, for everybody — and with it the whole class of bug where one mode
;;; remembered and another did not.
;;;
;;; What they replace is the copy per file. `split-string' was written out twice
;;; — `%lsp-split' and `%ai-split', identical down to the docstring.
;;; `executable-find' lived in `ai.lisp' and was reached from `tutor.lisp' and
;;; `math-written.lisp' through an `(fboundp 'executable-find)' guard, because
;;; neither could be sure the AI file had loaded; a helper in the file everybody
;;; loads first needs no guard, and the two are gone. And `run-process' was
;;; written twice, in `tutor.lisp' to mark an exercise in a child `ecl' and in
;;; `math-written.lisp' to hand a photograph to `curl' — the second with a
;;; comment saying it is the same shape as the first, which is the moment to
;;; merge rather than to note it. Two copies of a loop this fiddly is one copy
;;; that gets fixed and one that does not.

(defun split-string (string char)
  "STRING split on CHAR. Empty fields are kept; the caller drops them."
  (loop with start = 0
        for i = (position char string :start start)
        collect (subseq string start i)
        while i do (setf start (1+ i))))

(defun executable-find (program)
  "Where PROGRAM is on $PATH, as a pathname, or NIL.
A name containing a separator is a path and is taken as one, which is what
every shell does."
  (if (find #\/ program)
      (probe-file program)
      (dolist (dir (split-string (or (ext:getenv "PATH") "") #\:))
        (when (plusp (length dir))
          (let ((path (probe-file (concatenate 'string dir "/" program))))
            ;; `probe-file' answers for a *directory* of that name too, and its
            ;; truename has a NIL name component — which is how a directory
            ;; called `claude' on $PATH would otherwise read as an installed
            ;; harness.
            (when (and path (pathname-name path))
              (return path)))))))

(defun run-process (program args &key stdin (timeout 30))
  "Run PROGRAM with ARGS and wait for it. Answers (values OUTPUT STATUS), where
STATUS is :EXITED, :TIMEOUT or :BROKEN and OUTPUT is everything the child said.

The loop does three things at once, and each of them is a reason this is not
`ext:run-program' with `:wait t'. It drains the pipe *as it fills*, because a
chatty child that fills it blocks in `write' and then never reaches the exit
being waited for — a hang built out of two things that are each individually
correct. It polls for that exit rather than blocking on it, which is
`external-process-wait' with a NIL second argument. And it gives up at a
deadline, because ECL has no timeout of its own and this is the whole of adding
one; `terminate-process' with a true second argument is SIGKILL.

STDIN, when given, is written to the child and the pipe is then *closed* —
which is the point of it: `curl --config -' reads until end of file and would
otherwise wait for one forever.

stderr is merged into stdout, so a child that explains itself on the wrong
stream is still heard. That would corrupt a structured reply from a program
that wrote to both, and each caller here knows its program does not: curl's
`--silent --show-error' leaves only failures, which arrive *instead of* a body.

**This parks the Lisp thread until the child is done**, which is what TIMEOUT
is really bounding — nothing else in the image is evaluated meanwhile. A
program that can take minutes wants `:wait nil' and a poll on
`*point-moved-functions*' instead, which is why `project-clone-poll' in
`library.lisp' and `math-code-build-poll' in `math-code.lisp' are deliberately
*not* written in terms of this: a clone or a venv build would stall every
keystroke's `after-change-hook' for its whole length."
  (handler-case
      (multiple-value-bind (stream code process)
          (ext:run-program program args
                           :input (if stdin :stream nil)
                           :output :stream :error :output :wait nil
                           :external-format :utf-8)
        (declare (ignore code))
        (unwind-protect
             (progn
               (when stdin
                 ;; With both directions streamed ECL answers a TWO-WAY-STREAM,
                 ;; and closing *that* would take the reply away with the
                 ;; request. Only the half the child reads is closed.
                 (let ((to-child (if (typep stream 'two-way-stream)
                                     (two-way-stream-output-stream stream)
                                     stream)))
                   (write-string stdin to-child)
                   (finish-output to-child)
                   (close to-child)))
               (let ((text (make-string-output-stream))
                     (deadline (+ (get-internal-real-time)
                                  (* timeout internal-time-units-per-second))))
                 (loop
                   (loop while (listen stream)
                         do (let ((c (read-char stream nil nil)))
                              (if c (write-char c text) (return))))
                   (unless (eq (ext:external-process-wait process nil) :running)
                     (loop for c = (read-char stream nil nil)
                           while c do (write-char c text))
                     (return (values (get-output-stream-string text) :exited)))
                   (when (> (get-internal-real-time) deadline)
                     (ext:terminate-process process t)
                     (return (values (get-output-stream-string text) :timeout)))
                   (sleep 0.02))))
          (ignore-errors (close stream))))
    (serious-condition (e)
      (values (ignore-errors (princ-to-string e)) :broken))))

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
    (text-width            set-text-width            0)
    (font-size             set-font-size             18)
    ;; T because the editor starts with it on — see `Settings::scroll_past_end'.
    ;; Claimable like any other, which is what a mode showing a *generated*
    ;; document in an ordinary buffer wants: core exempts the generated *kinds*
    ;; on its own, and a listing rendered into a plain buffer is not one of them.
    (scroll-past-end       set-scroll-past-end       t))
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

This function owns *entering* a mode: the departing mode's exit hook, the
bodies, the auto-mode dispatch, and the claims that come with them. Switching
buffers enters no mode — the buffer was already in the one it is in, and its
body ran when it got there — so that half is `buffer-switch-hook' below, which
re-resolves the claims and does nothing else. Between them the editor's two
reports about which mode's settings should be on screen are covered, and
neither fires for the other's event."
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

(defun %set-minor-mode (name on)
  "Put NAME in state ON in the live buffer and run whichever of its bodies
applies. Answers ON, so a caller can use it as its own value.

The bodies run here rather than from the `NAME-on-hook' the editor pushes: that
hook is delivered on the next turn of the *application* loop, and this function
is the only thing that switches a minor mode, so running it on the spot is both
immediate and true in a headless image. The cost is that a bare
`(set-minor-mode \"visual-line\" t)' — the *primitive*, one word shorter than
this — flips the mode without running its body or re-resolving the settings it
claims. Call the mode's own command, or `enable-minor-mode'."
  (set-minor-mode name on)
  (let ((bodies (gethash name *minor-mode-bodies*)))
    (when bodies (funcall (if on (car bodies) (cdr bodies)))))
  (%refresh-settings)
  on)

(defun %toggle-minor-mode (name)
  "Flip NAME in the live buffer. The body of every command `define-minor-mode'
generates, and the only one of the two switches that announces itself: you
pressed a key to get here."
  (let ((on (%set-minor-mode name (not (minor-mode-p name)))))
    (message (format nil "~a ~:[off~;on~]" name on))))

(defun enable-minor-mode (mode)
  "Turn MODE on in the live buffer unless it is already on. T when this call is
what turned it on, NIL when it was on already.

The verb the mode system was missing, and four modes each wrote out in full
before it existed. A minor mode that belongs to a kind of file switches itself
on from a *mode hook* — `math-code' from `*python-mode-functions*',
`math-curriculum', `tutor-lesson' and `org-modern' from `*org-mode-functions*'
— and a mode hook runs on **every** entry into the mode, so a hook that called
the mode's own command would switch it back off the second time round. Hence
`(unless (minor-mode-p 'x) (x))' at four sites, each under its own paragraph
explaining the same guard. One paragraph is enough, and this is it.

Silent, unlike the toggle: a mode that came on because you opened the kind of
file it is for has not answered a question you asked, and each of those four
callers had a `message' of its own that the toggle's was overwriting anyway."
  (let ((name (%mode-name mode)))
    (unless (minor-mode-p name)
      (%set-minor-mode name t))))

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
;;; The three things the editor reports about a buffer
;;;
;;; `after-change-hook' says the *document* moved — you typed, or Lisp wrote
;;; into the buffer. `point-moved-hook' says *point* did — you pressed `j', or
;;; `w', or clicked. `buffer-switch-hook' says a *different buffer* is on screen
;;; — the switcher, a split you moved focus into, a file you opened that was
;;; already open. The application queues all three the same way, through
;;; `pending_hooks' and behind the same `fboundp' guard, so an image that defines
;;; none of them pays nothing for any.
;;;
;;; Both live here, and the first of them only arrived on the second attempt.
;;; `after-change-hook' was declared in `lsp.lisp' because the LSP client was
;;; the only thing that wanted it, and by the time `org-modern.lisp',
;;; `show-paren.lisp', `org-frozen.lisp', `math-code.lisp' and `org-latex.lisp'
;;; wanted it too there were *two* copies of the dispatcher — the second one
;;; wrapped in `(unless (fboundp ...))' so that whichever file loaded first won
;;; — and a defensive DEFVAR in every file that pushed onto either list. That is
;;; what a hook living in whichever feature happened to want it first costs, and
;;; it is why this is the file it belongs in: the standard library, beside
;;; `define-derived-mode', where a mode looks for it anyway.
;;;
;;; A function on either list runs on **every** keystroke at worst. So the rule
;;; for what goes on one is the rule org-appear already follows: a constant
;;; amount of work, or a cheap test that decides whether to do any. A buffer
;;; rescan here would be felt.
;;;
;;; DEFVAR and guarded DEFUNs, all on purpose: a config reload must not throw
;;; away the functions already registered, and must not replace a hook a config
;;; wrote for itself.

(defvar *after-change-functions* nil
  "Functions called with no arguments after any change to the live buffer.")

(defvar *point-moved-functions* nil
  "Functions called with no arguments after point moves in the live buffer.")

(defvar *buffer-switch-functions* nil
  "Functions called with no arguments after the buffer on screen changes.")

;;; IGNORE-ERRORS per function, in all three: one config's broken hook must cost
;;; you that hook rather than the next one in the list, and must not turn every
;;; keystroke into a backtrace.

(unless (fboundp 'after-change-hook)
  (defun after-change-hook ()
    (dolist (f *after-change-functions*) (ignore-errors (funcall f)))))

(unless (fboundp 'point-moved-hook)
  (defun point-moved-hook ()
    (dolist (f *point-moved-functions*) (ignore-errors (funcall f)))))

(unless (fboundp 'buffer-switch-hook)
  (defun buffer-switch-hook ()
    "The other half of `%enter-major-mode', and the whole of what a buffer
switch has to do: the settings a mode claims are global in the editor, so the
mode of the buffer you are now looking at is the one they must be resolved
from. Before this hook existed they followed the last mode *entered*, and
visiting a single `.rs' file cost every org buffer its wrapping and its measure
for the rest of the session.

Two lines because `%effective-setting' already recomputes from the bottom up
and `minor-modes' already answers from the live buffer — the only thing that
was ever stale is `*major-mode*', and the only thing needed is the truth.

No exit hook and no mode bodies, unlike an entry, and that is the decision
rather than an omission: a switch enters no mode. The buffer was already in the
mode it is in and its body ran when it got there, so re-running it would fire
`org-mode-exit-hook' every time you glanced at another file and would re-do
each mode's entry work per glance. `%enter-major-mode' still owns all of that
and still fires when a buffer's mode actually changes underneath you.

Rejected: hanging the resync on `*point-moved-functions*', which the editor
already runs on a buffer switch. It runs on every keystroke as well — the one
thing the list above says not to do — and it is driven by point *moving*, so
two buffers whose cursors sit at the same offset would switch between each
other in silence."
    (setf *major-mode* (%mode-name (major-mode)))
    (%refresh-settings)
    (dolist (f *buffer-switch-functions*) (ignore-errors (funcall f)))))

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
