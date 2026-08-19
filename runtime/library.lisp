;;; zemacs standard library — the machinery a config calls.
;;;;
;;;; This file ships with the editor and is upgraded with it. `init.lisp' is
;;;; yours and lives in `~/.zemacs.d/'; everything it *calls* lives here, so a
;;;; config you wrote a year ago keeps working while the implementation under it
;;;; goes on improving. That is the whole reason the two are separate files:
;;;; `set-face', `define-leader', `project-make' and `refresh-commands' are not
;;;; preferences, they are the vocabulary preferences are written in.
;;;;
;;;; The rule for deciding which file a form belongs in: if changing it would
;;;; change *your* editor, it is config; if changing it would change *every*
;;;; zemacs, it is library. A theme name is config. `load-theme' is library.
;;;;
;;;; Loaded by `init.lisp' before anything else, and it loads `modes/modes.lisp'
;;;; itself — see below for why that has to happen this early.
;;;;
;;;; Nothing here binds a key, sets a colour or picks a theme. Those are the
;;;; config's decisions and this file must not make them behind its back.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; Where the shipped Lisp lives
;;;
;;; `*runtime-dir*' is booted by the editor, in `PATHS_FORM' in
;;; `crates/lisp/src/shim.c', out of `$ZEMACS_RUNTIME' — which the application
;;; sets to the directory this file is in before it starts the image. It is *not*
;;; derived from where the config was loaded from, and that distinction is the
;;; entire point: a config in `~/.zemacs.d/' has no themes/ or modes/ next to it,
;;; and a `merge-pathnames' against its own directory would find nothing.
;;;
;;; `init.lisp' falls back to its own directory when the variable is unset, which
;;; is what keeps the test suite — which loads `runtime/init.lisp' by absolute
;;; path with no application around it — working unchanged.

(unless (boundp '*runtime-dir*)
  (defparameter *runtime-dir* nil
    "Directory of the shipped Lisp library. Set by the editor; see above."))

(defun runtime-file (name)
  "NAME inside the shipped runtime directory, or NIL if we cannot tell where it
is. Every caller degrades to doing nothing rather than to guessing."
  (and *runtime-dir* (merge-pathnames name *runtime-dir*)))

;;; ---------------------------------------------------------------------------
;;; Modes, first of all
;;;
;;; `modes/modes.lisp' is the major/minor mode machinery — `define-derived-mode',
;;; `set-mode-local', `add-hook' and the two hook lists the editor reports a
;;; buffer's changes on. It is loaded here, at the top of the library, for three
;;; reasons:
;;;
;;;   * `add-hook' has to exist before anything below joins a hook list, and
;;;     `project-clone-poll' does exactly that a few hundred lines down.
;;;   * It must be read before any `<mode>-hook' is *defined*, because
;;;     `define-derived-mode' generates those — a hand-written one afterwards
;;;     would replace the generated one and quietly detach the machinery.
;;;   * `%wrap-setting-primitives' takes over `set-tab-width' and its siblings so
;;;     that a plain call in a config records the *baseline* a mode-local setting
;;;     reverts to. The ponytail note on that function says a setting changed
;;;     before this file loads is not seen, and asks for the load to go above the
;;;     appearance settings. Here is above them.
;;;
;;;   (define-derived-mode NAME PARENT &body BODY)
;;;   (define-minor-mode NAME DOC (:on ...) (:off ...))
;;;   (set-mode-local MODE SETTING VALUE)   reverts when the mode is left
;;;   (define-mode-key MODE KEYS COMMAND)   inherited by derived modes
;;;   (add-auto-mode SUFFIX MODE)           pick a mode from the file name
;;;   (derived-mode-p MODE &optional OF) (minor-mode-p MODE)
;;;   (enable-minor-mode MODE)              on if off; what a mode hook calls,
;;;                                         since the mode's own command toggles
;;;
;;;   (add-hook '*after-change-functions* 'my-function)   the document moved
;;;   (add-hook '*point-moved-functions*  'my-function)   point moved

(let ((path (runtime-file "modes/modes.lisp")))
  (when path (load path :verbose nil :print nil)))

;;; ---------------------------------------------------------------------------
;;; Is the editor new enough to read this config?
;;;
;;; An installed binary keeps reading the runtime out of the checkout it was
;;; built from — see `scripts/install.sh', which says so and means it — so the
;;; two drift apart the moment a primitive is added here and the install is not
;;; re-run. Loading then dies at the first call to something the binary does not
;;; have, and everything below that line silently never happens.
;;;
;;; That failure is nasty out of all proportion to its cause, because what you
;;; see depends entirely on *where* the file stopped. A missing `zemacs-file'
;;; kills the config at the scratch-file definition — which is above the
;;; dashboard and above the keymap, so the logo does not appear and no key is
;;; bound, and neither symptom mentions a version. The status line does say
;;; "The function ZEMACS::ZEMACS-FILE is undefined", and that is a sentence
;;; nobody reads as "your application is out of date".
;;;
;;; So: check first, name the cause, say what to do about it.

(defparameter *required-primitives*
  '("zemacs-file" "set-face-style" "%dashboard-item")
  "Host functions this library calls that older builds do not have. There is no
need to list the ones that have always existed.")

(defun check-required-primitives ()
  "Say so, once, if the binary predates this library."
  (let ((missing (remove-if (lambda (name)
                              (let ((s (find-symbol (string-upcase name) :zemacs)))
                                (and s (fboundp s))))
                            *required-primitives*)))
    (when missing
      ;; A message and not an error: the editor is running and mostly usable, and
      ;; refusing to load the rest of the config would take the theme away too.
      (message (format nil "zemacs is older than this config — no ~{~a~^, ~}. ~
                            Run scripts/install.sh, then restart."
                       missing)))))

(check-required-primitives)

;;; ---------------------------------------------------------------------------
;;; `interactive' — where M-x gets a command's arguments
;;;
;;; M-x sends the name you picked into the image as `(name)', with no arguments,
;;; and that is the whole reason the offered list has always been the
;;; *zero-argument* functions: offering `set-scale' would produce a
;;; wrong-number-of-arguments error the moment you ran it. So `set-scale',
;;; `set-language' and `lsp-register-server' were useful commands you could not
;;; reach. `load-theme' escaped by taking its argument `&optional' and *asking*
;;; when it was missing, which is what Emacs' own `load-theme' does — and that
;;; trick, generalised, is this section:
;;;
;;;   (defun set-scale (n) ...)
;;;   (interactive set-scale ("Font size: " :number))
;;;
;;;   (set-scale 20)   ; from a config or another function: called, unchanged
;;;   (set-scale)      ; from M-x or a key: asks for `n', then calls
;;;
;;; `interactive' replaces the symbol's function with a wrapper that dispatches
;;; on *whether it was given any arguments*. Nothing in core changes, nothing in
;;; the shim changes, and a command that takes no arguments is not touched at
;;; all: it is still offered by arity, still called as `(name)', still runs with
;;; no prompt in front of it. That last point is the one worth protecting — the
;;; regression nobody would forgive is `M-x quit' growing a question.
;;;
;;; Wrapping a function that is already there is the same move `which-key.lisp'
;;; makes on `register-command', and it is why this needed no new mechanism.
;;;
;;; Rejected, in the order they were tried:
;;;
;;;   * **Emacs' `(interactive "sTheme: ")' inside the body.** Emacs can read a
;;;     function's body back; ECL cannot, so the declaration has to sit outside
;;;     the `defun' whatever it looks like. Once it is outside, the code-letter
;;;     alphabet is a second language to learn where a keyword says the same
;;;     thing, and `advice' would still be how it took effect.
;;;
;;;   * **Typing the arguments on the M-x line**, `M-x set-scale 20'. The
;;;     candidate rows carry a docstring and a key — `%annotated-command' in
;;;     `modes/which-key.lisp' — so core takes the *first word* of the row as
;;;     the command name, and an argument and an annotation are the same thing
;;;     to it. Recovering one would mean a Lisp reader in core, which is the
;;;     wrong side of the boundary for parsing Lisp.
;;;
;;;   * **No declaration at all: prompt for every required parameter, reading
;;;     the lambda list.** The parameter names are already introspectable and
;;;     they make good labels — which is why they *are* the default label below.
;;;     But a name is all they are. It cannot say that `n' is a number or that a
;;;     language has eight legal values, so `set-scale' would be handed the
;;;     string "20" and `set-language' would stay the free-text footgun it is
;;;     hidden for. It would also offer every N-argument helper in the package,
;;;     which is the mess `*hidden-commands*' exists to clean up.
;;;
;;;   * **A `defcommand' defining form.** It would have to re-implement `defun's
;;;     docstring and declaration handling to buy one line of nesting, and a
;;;     command is still just a function.
;;;
;;;   * **A dispatcher in core: send `(%run-command "name")' instead of
;;;     `(name)'.** Then every key bound to a Lisp command depends on a function
;;;     defined in *this* file — which several tests, and any config that stops
;;;     early on an error, do not have. The wrapper needs nothing from core, and
;;;     that is the argument for it.
;;;
;;; ponytail: "called with no arguments" is this file's entire definition of
;;; "called interactively", where Emacs distinguishes `call-interactively' from
;;; `funcall'. It costs a command that means something with *none* of its
;;; arguments — `(set-language)' used to mean "plain text" and now asks, so that
;;; call is spelled `(set-language nil)'. Nothing else in the tree relied on the
;;; difference. The upgrade is a second entry point, not a change of shape.

(defparameter *lambda-list-fn* (find-symbol "FUNCTION-LAMBDA-LIST" "EXT")
  "ECL's introspection entry point, looked up rather than named literally so a
build without it still reads this file. Two things want it: the labels
`interactive' puts on its prompts, and the arity test in `%zero-arg-p'.")

(defun %arg-labels (sym n)
  "Prompt labels for SYM's first N parameters, taken from their own names.

`(defun set-scale (n) ...)' therefore asks `n: ', which is exactly why every
declaration in the tree overrides it: a parameter name is a fine default and a
poor prompt. Falls back to counting when there is no lambda list to read — a
host primitive is a C function and ECL has none for it."
  (let ((args (and *lambda-list-fn*
                   (ignore-errors (funcall *lambda-list-fn* sym)))))
    (loop for i from 0 below n
          for p = (nth i args)
          collect (if (and p (symbolp p) (not (member p lambda-list-keywords)))
                      (format nil "~(~a~): " p)
                      (format nil "argument ~d: " (1+ i))))))

(defun %interactive-value (answer source)
  "ANSWER as SOURCE describes it, or NIL meaning stop.

NIL in is a cancelled prompt, and that stops with nothing to say — Escape is not
an error. A `:number' that will not parse is the other stop, and that one
reports: the user typed something, and silence would look like it had run."
  (cond ((null answer) nil)
        ((eq source :number)
         (or (parse-integer answer :junk-allowed t)
             (progn (message (format nil "not a number: ~a" answer)) nil)))
        (t answer)))

(defun %read-args (prompts specs acc k)
  "Ask for each of SPECS in turn, then call K with the answers as a list.

The recursion *is* the nesting the continuation model forces on us:
`read-string' cannot return an answer (see docs/threading.org), so N arguments
are N callbacks inside one another. Written as a tail call they are one copy of
these lines instead of N levels of hand-written indentation, and that is the
whole readability argument for having a collector at all — it is worth little at
one argument and a lot at three.

Cancelling stops here and K is never called, so a command that was going to ask
three questions and got one answer does *nothing*. Collecting the arguments and
then running the command once is the same rule `docs/threading.org' states for
edits: build the whole answer first, apply it in one go.

A spec that named a `:preview' form gets consult's live preview, and the whole
of what makes one is `APPLY-WITH' below: running the command with the
highlighted candidate in the slot being asked for. K *is* the command, so a
preview costs no second description of what the command does and cannot drift
from it — `load-theme' previews by loading the theme, which is the only
definition of \"show me that theme\" there could be.

BEFORE is what the `:preview' form answered at the moment the prompt opened,
and cancelling applies it again. Escape has to be a round trip or a preview is a
trap rather than a preview."
  (if (null specs)
      (funcall k (nreverse acc))
      (let* ((spec (first specs))
             (label (or (first spec) (first prompts)))
             (source (second spec))
             (was (third spec))
             (more-specs (rest specs))
             (more-prompts (rest prompts))
             (apply-with (lambda (v) (funcall k (append (reverse acc) (list v)))))
             ;; Last argument only: with more questions still to come the other
             ;; arguments are not known yet, so there is nothing to apply.
             (before (and was (null more-specs) (functionp source) (funcall was)))
             (step (lambda (answer)
                     (let ((v (%interactive-value answer source)))
                       (cond (v (%read-args more-prompts more-specs (cons v acc) k))
                             (before (funcall apply-with before)))))))
        (if (functionp source)
            (completing-read label (funcall source) step
                             (and before apply-with))
            (read-string label step)))))

(defun %interactive (sym specs)
  "Make `(SYM)' with no arguments ask for SPECS and then call SYM with them.

SPECS is a list of `(LABEL . SOURCE)', where LABEL may be NIL to take the
parameter's own name and SOURCE is `:string', `:number', or a function of no
arguments answering a list of completion candidates.

The entry point under the `interactive' macro, and a function rather than only a
macro on purpose: a file that cannot count on this one having been loaded calls
it behind an `fboundp' guard, where an undefined *macro* at the top level would
take the rest of that file with it. `runtime/lsp.lisp' is the case — the LSP
tests load it without the library."
  (let ((fn (symbol-function sym))
        (doc (ignore-errors (documentation sym 'function)))
        (prompts (%arg-labels sym (length specs))))
    (setf (get sym 'interactive) specs)
    (setf (symbol-function sym)
          (lambda (&rest args)
            (if args
                (apply fn args)
                ;; NIL and not what `%read-args' answered, which is the *prompt
                ;; id* `read-string' hands back. `eval-string' echoes the value
                ;; of the last form it evaluated unless that value is NIL — see
                ;; docs/threading.org — so returning the id put a bare `1' in the
                ;; status line the instant `M-x load-theme' opened its prompt.
                ;; Every command that has already asked its question returns NIL
                ;; for the same reason.
                (progn (%read-args prompts specs nil
                                   (lambda (vals) (apply fn vals)))
                       nil))))
    ;; Carried across by hand: `setf symbol-function' installs a *new* function
    ;; object and the docstring lived on the old one. The docstring is what M-x
    ;; prints beside the name (`command-annotation'), so losing it would make
    ;; every command this touches the one row in the list with nothing to say.
    (when doc (setf (documentation sym 'function) doc))
    sym))

(defmacro interactive (name &rest specs)
  "Declare how M-x should obtain NAME's arguments. One SPEC per argument:

  :string   free text
  :number   free text, read as an integer
  FORM      evaluated when the prompt opens; the list of strings it answers are
            the completion candidates, and nothing is required to match

Any of the three may be written `(LABEL SPEC)' to say what the prompt should
read; without a LABEL it is the parameter's own name.

A candidate spec may add `:preview FORM' — `(LABEL SPEC :preview FORM)' — which
turns the prompt into a live one: scrolling the list *runs the command* on the
highlighted candidate, so you see the theme rather than its name. FORM is
evaluated when the prompt opens and must answer what is showing *now*, because
that is what cancelling puts back. Only the last argument of a command can
preview; there is nothing to run while other arguments are still unknown.

Put it directly under the `defun'. It captures the function that is there when
it runs, so the two travel together — a config reload re-reads the `defun' and
then re-wraps the fresh definition, in that order, which is what keeps a reload
from stacking wrappers on wrappers."
  `(%interactive
    ',name
    (list ,@(mapcar (lambda (s)
                      (let* ((label (and (consp s) (stringp (car s)) (car s)))
                             (tail (if label (rest s) (list s)))
                             (src (first tail))
                             (was (getf (rest tail) :preview)))
                        ;; A keyword stays a keyword; anything else is a form to
                        ;; evaluate *when the prompt opens* and not now, so
                        ;; `(theme-names)' re-reads the directory each time. The
                        ;; preview form is deferred for the same reason and read
                        ;; at the same moment.
                        `(list ,label
                               ,(if (keywordp src) src `(lambda () ,src))
                               ,(and was `(lambda () ,was)))))
                    specs))))

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

;;; One face, said once.
;;;
;;; `set-syntax-color' takes three floats and `set-face-style' takes two flags,
;;; and a theme that calls both for all 22 faces is 44 lines of which half say
;;; nothing. This is the spelling a theme actually wants:
;;;
;;;   (set-face "keyword"  '(0.78 0.57 0.94) :bold t)
;;;   (set-face "comment"  '(0.42 0.46 0.58) :italic t)
;;;   (set-face "modeline" '(0.16 0.18 0.26))
;;;
;;; A Lisp wrapper over two primitives rather than a third primitive, which is
;;; the standing rule: the editor learned exactly one new verb for weight, and
;;; the ergonomics are a `defun'. NIL colour styles a face without recolouring
;;; it, which is how a theme adds weight to a face it is happy with.
;;;
;;; Both flags are always sent, not only when true — a theme that leaves `:bold'
;;; off means *not bold*, and inheriting the last theme's weight is exactly the
;;; bug the exhaustive face lists exist to prevent.
;;;
;;; The slant is also remembered here, in `*face-italic*'. `set-face-style' sets
;;; weight and slant together and the editor publishes no reader for either, so
;;; anything that wants to make a face bold *afterwards* — which is exactly what
;;; `*bold-constructs*' below does — would otherwise straighten a face the theme
;;; had deliberately slanted. Tokyo Night's italic keywords are the case: forcing
;;; bold on them without this would silently undo the single thing that theme is
;;; most recognisable for. ponytail: a `(face-style face)' reader in the editor
;;; is the honest fix and is perhaps six lines of Rust; a hash table here needs
;;; none of them and is wrong only if something sets a style behind this
;;; function's back, which nothing does.
;;;
;;; RGB is required rather than `&optional', even though NIL is a legal value
;;; for it: `&optional' before `&key' is a Common Lisp trap, and this is the
;;; exact call that springs it. `(set-face "keyword" :bold t)' would bind RGB to
;;; the keyword `:bold' and then find `t' where a keyword name should be, which
;;; is a confusing error a long way from the theme file that caused it. Required
;;; means the mistake is a wrong-number-of-arguments at load time instead.
(defparameter *face-italic* (make-hash-table :test #'equal)
  "The slant last asked for, per face name. See `set-face'.")

(defun set-face (name rgb &key bold italic)
  "Colour and weight the face called NAME.
RGB is a (R G B) list of floats, or NIL to leave the colour alone."
  (when rgb (apply #'set-syntax-color name rgb))
  (setf (gethash name *face-italic*) (and italic t))
  (set-face-style name (and bold t) (and italic t))
  name)

;;; Bold constructs.
;;;
;;; Almost none of the ported themes bold anything in code, and that is faithful
;;; rather than lazy: Catppuccin's style guide has no font-style section at all,
;;; folke's Tokyo Night defaults `styles.keywords` to no weight, and Modus ships
;;; `modus-themes-bold-constructs' set to nil. Every one of them carries meaning
;;; in hue, on the assumption that the reader wants the page calm.
;;;
;;; That is a taste, and it is a taste about *their* editor rather than about
;;; this one. So it is a variable, the way it is a variable in Modus: the themes
;;; stay honest ports, and the weight the reader wants is applied on top of
;;; whichever one is loaded. `(setf *bold-constructs* nil)' in your init gives
;;; you the ports exactly as published.
;;;
;;; Applied after the theme file rather than inside it, which is why a theme
;;; needs to know nothing about this: `load-theme' is the only door in.
(defparameter *bold-constructs* '("keyword" "function" "type")
  "Faces made bold after any theme loads, whatever that theme asked for.
NIL for the themes as their authors published them. A short list on purpose —
a screen where everything is heavy has no emphasis left to give.")

(defparameter *current-theme* nil
  "The theme that is showing, or NIL before the config has loaded one.

Tracked here rather than asked of the editor because there is nothing to ask: a
theme *is* a file of `set-background' and `set-syntax-color' calls, so once it
has run there is no name left anywhere — only the colours it set. `load-theme'
is the one door in, which is what makes one variable beside it sufficient.

Its reason for existing is the preview on `load-theme's prompt: scrolling the
theme list loads each theme in turn, and cancelling has to put back the one you
started with.")

(defun %apply-bold-constructs ()
  "Re-assert weight on `*bold-constructs*', preserving each face's slant."
  (dolist (face *bold-constructs*)
    (set-face-style face t (gethash face *face-italic*))))

;;; Why a theme change starts by throwing everything away.
;;;
;;; A theme is a pile of assignments into a table, so a face the incoming theme
;;; does not mention keeps the *outgoing* theme's answer. For the 22 faces about
;;; text that is closed by making the pile total — every shipped theme names all
;;; 22, and `crates/lisp/tests/themes.rs' fails the build if one does not.
;;;
;;; The UI faces cannot be closed that way, because they are deliberately
;;; optional: `cursor', `region', `popup' and the rest fall back to the ratios
;;; the renderer was mixing before they existed, which is what let eleven themes
;;; become cursor-themeable without one of them being edited. An optional face
;;; is exactly the face a test cannot insist on — so the bleed is stopped at the
;;; other end instead. Empty the table, then load; anything the new theme is
;;; silent about falls through to a ratio of *its own* ground and body colour,
;;; which is right no matter what was showing a moment ago.
;;;
;;; `*face-italic*' goes with it. It shadows a slant the editor publishes no
;;; reader for (see `set-face'), so a stale entry here is the same bug wearing a
;;; Lisp hat: `*bold-constructs*' would re-slant a face the new theme left
;;; upright, because the hash still remembered the old one asking for it.
(defun %forget-faces ()
  "Drop every face colour and style, here and in the editor."
  (clrhash *face-italic*)
  (reset-faces))

(defun load-theme (name)
  "Load theme NAME from the shipped themes/ directory.

With no NAME, ask — which is what makes this an `M-x' command as well as the
function a config calls. Emacs spells it the same way and for the same reason:
there is one door in, and whether you came through it with the answer already
in hand is not a second function's worth of difference.

The asking is the `interactive' declaration below and no longer a line in this
body. It used to be `(&optional name)' plus `(unless name (return-from ...))',
which is the hand-written version of the same thing — and was the precedent the
declaration was generalised out of."
  (let ((path (runtime-file (format nil "themes/~a.lisp" name))))
    (cond ((null path) (message "load-theme: cannot tell where the runtime lives"))
          ((probe-file path) (%forget-faces)
                             (load path :verbose nil :print nil)
                             (%apply-bold-constructs)
                             (setf *current-theme* (string name))
                             (message (format nil "theme: ~a" name)))
          (t (message (format nil "no such theme: ~a" name))))))

;;; Which themes exist is a question the *directory* answers, not a list kept in
;;; step with it by hand. There were three themes and three `defun's, and at
;;; eleven that trade stops being worth making: a list you have to remember to
;;; edit is a list that will be wrong, and the failure mode — a theme file that
;;; is present and unofferable — is invisible.
(defun theme-names ()
  "Every theme in the shipped themes/ directory, by name, sorted."
  (let ((glob (runtime-file "themes/*.lisp")))
    (when glob
      (sort (mapcar #'pathname-name (directory glob)) #'string<))))

;; Previewing, which is the whole of what makes a theme list usable: eleven names
;; tell you nothing about eleven themes. Escape puts back the one that was on.
(interactive load-theme ("Theme: " (theme-names) :preview *current-theme*))

(defun theme ()
  "Pick a theme. Bound to `SPC t t'.

Nothing but `(load-theme)' now that `load-theme' asks for itself. Kept as a name
rather than deleted because `SPC t t' and the dashboard's `t' are *strings* in
`init.lisp', and a config already seeded into someone's `~/.zemacs.d/' is not
ours to rewrite."
  (load-theme))

;;; ---------------------------------------------------------------------------
;;; Magnification, the way `text-scale-adjust' works in Emacs.

(defparameter *font-size* 16
  "Current point size. Kept here rather than read back from the editor, the
same way Emacs tracks `text-scale-mode-amount' in a variable. A config that
calls `(set-font-size n)' should `(setf *font-size* n)' beside it, or the first
`text-scale-increase' will step from this default rather than from what you
asked for — which is what `set-scale' is for.")

(defun set-scale (n)
  (setf *font-size* (max 6 (min 96 n)))
  (set-font-size *font-size*)
  (message (format nil "font size ~d" *font-size*)))

;;; ---------------------------------------------------------------------------
;;; The font itself, as opposed to its size.
;;;
;;; Which fonts exist is the one question here Lisp cannot answer: it takes a
;;; font library to tell a monospace face from a proportional one, and guessing
;;; from the filename offers you Helvetica and hides Iosevka. So the editor is
;;; asked — `(%do "list-fonts")' — and answers on a later turn by calling
;;; `%fonts-listed' with the pairs. Everything after that is here.

(defparameter *fonts* nil
  "`(FAMILY . PATH)' for every monospace font installed, or NIL before the
editor has been asked. Filled by `%fonts-listed'.")

(defparameter *current-font* nil
  "The family showing now, or NIL for whichever one the editor picked at
startup. What `choose-font's preview puts back when you cancel.")

(defparameter *font-prompt* nil
  "The thunk waiting on the scan, when something asked before the list existed.

One slot and not a queue: what waits is a prompt, and there is one of those. A
second `M-x choose-font' before the first has drawn replaces it, which is what
the editor does with the prompt itself.")

(defun %fonts-listed (pairs)
  "Called by the editor with the scan it was asked for. See `list-fonts'."
  (setf *fonts* pairs)
  (let ((then (shiftf *font-prompt* nil)))
    (when then (funcall then)))
  nil)

(defun list-fonts (&optional then)
  "Answer the installed monospace families, asking the editor if need be.

THEN is called once the list is in hand — immediately when it already was, and
on a later turn of the Lisp queue when the editor had to be asked. That is the
same continuation shape `read-string' has and for the same reason: nothing here
may block the image (`docs/threading.org').

The scan costs a few hundred file opens, so it is done once a session. `(setf
*fonts* nil)' forces a fresh one after installing a font."
  (cond (*fonts* (when then (funcall then)) (mapcar #'car *fonts*))
        (t (setf *font-prompt* then)
           (%do "list-fonts" "" 0 0)
           nil)))

(defun set-font (family)
  "Set the body font to FAMILY, one of `list-fonts'.

The editor is handed a *path*: it has no font library to resolve a name with,
and the name-to-path table is the scan this file already holds. NIL goes back to
whichever font the editor finds for itself, which is what an empty path means."
  (if (null family)
      (progn (setf *current-font* nil) (%do "font-path" "" 0 0))
      (let ((hit (assoc (string family) *fonts* :test #'string-equal)))
        (cond ((null hit) (message (format nil "no such font: ~a" family)))
              (t (setf *current-font* (car hit))
                 (%do "font-path" (cdr hit) 0 0)
                 (message (format nil "font: ~a" (car hit))))))))

(defun choose-font ()
  "Scroll the installed monospace fonts, with the editor set in each as you go.

`M-x choose-font'. The preview is the point — a list of font names is exactly as
useful as a list of theme names, which is to say not at all — so this is
`load-theme's prompt with a different list behind it, and Escape puts back the
font you were using.

Hand-written rather than declared with `interactive' `:preview' because the
list has to be *fetched* before the prompt can open, and `interactive' evaluates
its source form at that moment and expects the answer there and then."
  (list-fonts
   (lambda ()
     (let ((before *current-font*))
       (completing-read "Font: " (mapcar #'car *fonts*)
                        (lambda (answer)
                          (set-font (or answer before)))
                        (lambda (candidate)
                          (when candidate (set-font candidate)))))))
  nil)

;;; `set-font-size' is deliberately *not* declared interactive, though it is the
;;; primitive and this is the wrapper. It sets the size and nothing else, so an
;;; `M-x set-font-size 30' would leave `*font-size*' at whatever it was and the
;;; next `text-scale-increase' would step from the stale number — the exact trap
;;; the docstring above warns configs about. `set-scale' is the same gesture done
;;; correctly, and it is the one that is now reachable.
(interactive set-scale ("Font size: " :number))

(defun text-scale-increase () (set-scale (+ *font-size* 2)))
(defun text-scale-decrease () (set-scale (- *font-size* 2)))
(defun text-scale-reset    () (set-scale 22))

;;; ---------------------------------------------------------------------------
;;; Setting the language by hand
;;;
;;; `set-language' is a shim `defun' taking `&optional language', so it has always
;;; been zero-argument by introspection and has always been in `*hidden-commands*'
;;; — because the zero-argument call means *plain text*, and `M-x set-language'
;;; landed on by a stray fuzzy match silently uncoloured the buffer. Asking is the
;;; whole fix, and it is what takes the name back off the hidden list.

(defparameter *languages*
  '("rust" "lisp" "python" "json" "toml" "c" "javascript" "org")
  "What tree-sitter can colour, for completion only.

ponytail: a hand-kept copy of `LANGS' in `crates/syntax/src/lib.rs'. Core has no
reader for it and cannot grow one without depending on the syntax crate, which is
a dependency edge worth more than a completion list. Drift is harmless in the
direction that matters — nothing is required to match, so a language added to the
build is still typeable here — and the upgrade is a `language-list' reader.")

(interactive set-language ("Language: " *languages*))

;;; ---------------------------------------------------------------------------
;;; The config file itself
;;;
;;; `*config-file*' is set by `init.lisp', which is the only file that knows
;;; where it was loaded from. Everything that re-reads or opens the config goes
;;; through it, so a config anywhere on the disk — `~/.zemacs.d/init.lisp', the
;;; shipped default, or whatever `$ZEMACS_INIT' names — is reloadable and
;;; editable without anything here knowing which.

(defvar *config-file* nil
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

;;; The notes file, which is `edit-config' pointed at prose instead of Lisp: one
;;; path, reachable from every buffer and every mode, for the thing you noticed
;;; while you were busy doing something else. `init.lisp' binds it everywhere,
;;; because a notes file you can only reach from some buffers is one you stop
;;; trusting and therefore stop using.

(defparameter *todo-file* "~/Code/zemacs/TODO.org"
  "The file `open-todo' opens. `~/' is expanded, and the file is created if it is
not there.

One fixed path and deliberately not the current project's TODO.org: the whole
value of this key is that it goes to the *same* file wherever you press it, so a
complaint thought of while editing something else lands with the others rather
than in whatever directory you happened to be in. Set it in your init to keep
your notes somewhere else.")

(defun open-todo ()
  "Open `*todo-file*', creating it if this is the first note.

Created and not merely opened, because `find-file' on a path with nothing behind
it is an error in the status line rather than an empty buffer — and the single
moment anyone reaches for this is the moment they least want to be told to go
and make a file first. `:if-exists nil' so an existing file is opened and never
truncated: this command must be safe to hold down."
  (let ((path (%expand-home *todo-file*)))
    (with-open-file (s path :direction :output
                            :if-does-not-exist :create
                            :if-exists nil)
      (declare (ignore s)))
    (find-file path)))

(defun lisp-version ()
  "Prove there is a real Common Lisp in here."
  (message (format nil "~a ~a — ~d symbol~:p in ZEMACS"
                   (lisp-implementation-type)
                   (lisp-implementation-version)
                   (let ((n 0))
                     (do-symbols (s (find-package :zemacs)) (declare (ignore s))
                       (incf n))
                     n))))

;;; ---------------------------------------------------------------------------
;;; The scratch buffer
;;;
;;; Emacs's *scratch* has no file behind it. Ours does, because `find-file' is
;;; the only primitive that can put the editor in a *different* buffer —
;;; `insert' would drop a Lisp header into whatever you happened to be editing.
;;; A real .lisp file also gets syntax highlighting and survives a restart.

(defparameter *scratch-file* (zemacs-file "scratch.lisp")
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
;;; M-x calls the name you pick as `(name)', with no arguments. So the list is
;;; the functions for which that is a legal call — the zero-argument ones, plus
;;; the ones declared `interactive' above, which are zero-argument on purpose:
;;; their wrapper takes `&rest' and asks for what it needs.

;;; The host primitives are C functions: ECL has no lambda list for them and
;;; reports "unknown", so the filter below excludes all of them — including the
;;; zero-argument ones. These few are worth offering anyway.
(defparameter *extra-commands* '("quit" "show-dashboard")
  "Names published to M-x on top of what introspection finds. Modes push onto
this — `terminal-paste' and the `ai' family do — so it is a DEFPARAMETER that a
config may add to rather than replace.")

(defparameter *hidden-commands*
  (append (when (boundp '*readers*) (symbol-value '*readers*))
          ;; `load-theme' is deliberately *not* here: with no argument it asks,
          ;; so `M-x load-theme' is a real command and not a call that errors.
          ;; Neither is `set-language', which used to be — with no argument it
          ;; meant "plain text", so a stray fuzzy match silently uncoloured the
          ;; buffer, and it now asks for the language instead. `kill-buffer' was
          ;; never here either: no argument means the live buffer, which is
          ;; exactly what Emacs' `C-x k' does.
          '("make-marker" "point-marker" "theme-names" "runtime-file"
            "buffer-lines" "buffer-names" "beginning-of-line" "end-of-line"
            "check-required-primitives"
            ;; Zero-argument only in the sense that calling it with none means
            ;; "stop waiting for a keystroke". The useful call takes the function
            ;; the key goes to, and a bare one is a no-op — see `avy.lisp', which
            ;; is what a user reaches this through.
            "grab-key"))
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
  "Publish the functions of this package that `(name)' can call as M-x
candidates. Clears first, so reloading the config does not duplicate the list."
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
                 ;; The same promise arrived at two ways: nothing is required,
                 ;; or the arguments are asked for. Asked as a *property* rather
                 ;; than by re-introspecting — the wrapper's lambda list is
                 ;; `(&rest args)' and whether ECL reports one for a closure it
                 ;; did not compile is not a thing to bet the M-x list on.
                 (or (%zero-arg-p s) (get s 'interactive)))
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

Block-capital ASCII used to spell the name here, then letter-spaced type did,
and now neither does. The name is the one thing on this screen nobody needs
told: it is in the window title, it is what you typed to get here, and the logo
above already says which language the application is made of. What is left is
the four lines that say something you did not already know — what it is for,
one koan, and which image is actually running.

The art is gone for a second reason worth keeping written down: block-drawing
characters degrade into the wrong letters in a font that is missing some of
them, which is a worse first impression than no artwork at all.

No leading whitespace on any line: the dashboard centres each line itself, so
padding here would shift the block off-centre rather than move it."
  (let ((koan (nth (random (length *koans*)) *koans*)))
    (format nil "
a common lisp machine that edits text

;; ~a

(~a ~a) on ~a
"
            koan
            (string-downcase (lisp-implementation-type))
            (lisp-implementation-version)
            (string-downcase (software-type)))))

;;; The logo. `image-file' answers NIL when it cannot read the file, and
;;; `dashboard-logo' takes NIL to mean "no logo" — so a checkout without the
;;; assets directory falls back to the ASCII banner alone instead of leaving a
;;; hole where a lambda should be. That is the same contract `latex-preview'
;;; has, for the same reason: an asset is a thing that can be missing, and a
;;; config must survive it.
;;;
;;; Sized in ems, like every other figure, so it grows with the font rather than
;;; staying a fixed slab of pixels when the display or the point size changes.
(defun dashboard-default-logo (&optional (ems 10))
  "Put the shipped logo above the banner, at EMS ems wide. A no-op when the
assets directory is not there."
  (let ((path (runtime-file "../assets/Lisp_logo.svg.png")))
    (when path (dashboard-logo (image-file path ems)))))

(defun dashboard-item (key label action &optional hint)
  "Add a row: press KEY to run ACTION, shown as LABEL.
HINT is the key sequence printed dim on the right; it defaults to the leader
sequence bound to ACTION, and NIL means none. The editor primitive underneath
takes four arguments and this takes three or four, which is the only reason
this exists — see `%dashboard-item'.

The default is what makes the startup screen teach the keymap instead of being
a menu you use once: every row prints the leader sequence that runs the same
command, looked up rather than typed out beside the label. Written twice it
would be wrong within a month — someone moves `SPC f f' and the dashboard goes
on advertising the old one, which is the exact failure a printed hint is
supposed to prevent. It follows that the items must be built *after* the
bindings; a menu built first prints a column of blanks."
  (%dashboard-item key label action (or hint (leader-key action) "")))

;;; ---------------------------------------------------------------------------
;;; The modeline
;;;
;;; What the strip says is a list of little templates set from here; the editor
;;; expands them per pane per frame. That division is the point: a callback per
;;; frame is what makes a slow `mode-line-format' in Emacs make the whole editor
;;; feel slow, and `docs/threading.org' is a document about never doing that. So
;;; the *shape* is yours and costs nothing, and the expansion is a scan of a few
;;; dozen bytes in Rust.
;;;
;;; The codes, which `crates/core/src/modeline.rs' documents in full:
;;;
;;;   %m  the modal state, active pane only    %M  the major mode, as a word
;;;   %b  the buffer's name                    %n  the minor modes
;;;   %f  its path                             %l  the line, %c the column
;;;   %+  ● when there are unsaved changes     %p  Top / Bot / All / a percentage
;;;   %r  ◈ when it is read-only               %P  unix permissions
;;;   %s  the last message, active pane only   %%  a literal %
;;;   %k  the half-typed key sequence
;;;
;;; **A segment whose codes all come back empty is dropped whole**, which is the
;;; whole of the conditional logic and is why no code needs an `if' around it:
;;; `"  %P"' carries its own two spaces and leaves with them on a buffer that has
;;; no file behind it.

(defun %face-number (face)
  "FACE as the editor spells one on the modeline.

NIL is the strip's own colour, `:mode' is *the colour of the mode you are in* —
the one face that cannot be named ahead of time, since it is a different one in
each state — and anything else is a name from `face-list', the same vocabulary
an overlay takes."
  (cond ((null face) -1)
        ((eq face :mode) -2)
        ((integerp face) face)
        (t (or (position (string-downcase (string face)) (face-list)
                         :test #'string=)
               (error "modeline: no such face: ~a" face)))))

(defun clear-modeline ()
  "Take both sides of the modeline down, to build one from scratch."
  (%do "modeline-clear" nil 0 0)
  nil)

(defun modeline-segment (side template &key face bold filled)
  "Add TEMPLATE to the modeline's SIDE — `:left' or `:right'.

FACE is a name from `face-list', `:mode', or NIL for the strip's own colour.
BOLD stacks with whatever weight the theme gave that face rather than replacing
it: the flag is the modeline's *structural* emphasis — the buffer name is bold
because it is the buffer name — while the theme's is a claim about the face, and
both are true at once.

FILLED paints the face as a block behind the text and knocks the text out to the
strip's colour. It is what makes the mode indicator a shape rather than a word:
a colour chosen to be legible *on* the bar is by construction not legible *as*
the bar, so the ink has to swap to the ground it is now sitting on."
  (%do "modeline-segment" (string template)
       (logior (if (eq side :right) 1 0)
               (if bold 2 0)
               (if filled 4 0))
       (%face-number face))
  nil)

(defvar *modeline-note-functions* nil
  "Functions called with the note's text when the `%N' segment is clicked.

The one segment whose meaning this file cannot know: the note belongs to
whichever mode put it there, so that mode is what has anything to say about it.
Each is called with what the strip is showing and answers a string to say
instead, or NIL to pass. The first answer wins; if nobody answers, the note
itself is repeated — which is worth doing on its own, since the strip truncates
and a narrow pane shows half of it.

DEFVAR and not DEFPARAMETER: reloading a config must not throw away the
handlers the modes already registered.")

(defun modeline-note (text)
  "Put TEXT on the modeline wherever the strip has a `%N', or take it down when
TEXT is NIL or empty.

For a *mode* with a standing fact to report — a watcher's backlog, a job in
flight — as opposed to `message', which is for the thing that just happened and
is gone by the time anyone looks. A `%N' segment disappears entirely while the
note is empty, so a mode that is quiet costs nothing on the strip."
  (%do "modeline-note" (or text "") 0 0)
  nil)

(defun default-modeline ()
  "Build the strip the editor ships with.

Here rather than in `init.lisp' because this file is loaded from the *runtime*
directory whatever config is running, and that one is the copy in your
`~/.zemacs.d/' the moment it exists — so a config written before the modeline
became configurable would otherwise boot into the two-segment fallback the
editor keeps for a headless session. Your init runs after this and may
`clear-modeline' and say something else entirely; that is the whole point."
  (clear-modeline)

  ;; Left: what you are doing, and to what.
  ;;
  ;; The pill first, in the colour of the state, because modal editing's one
  ;; recurring cost is losing track of which mode you are in and the thing that
  ;; fixes it is a shape at the left edge that is a different colour in each —
  ;; recognisable at the edge of vision, which coloured text on a dark strip is
  ;; not. The spaces are its padding: the strip is a character grid, so a pill is
  ;; as wide as its label and the label carries its own margin.
  (modeline-segment :left " %m " :face :mode :bold t :filled t)
  (modeline-segment :left "  %b" :bold t)
  ;; `warning' rather than bold for the dot. Bold is a near-white in most themes
  ;; — the same near-white the buffer name beside it is already set in — so the
  ;; one mark on the strip meaning "you have work that is not on disk" was drawn
  ;; in the colour of everything around it.
  (modeline-segment :left " %+" :face "warning")
  (modeline-segment :left " %r" :face "comment")
  ;; The message and the half-typed key sequence, both transient, both on the
  ;; left after the file — pushing the position around as they come and go is
  ;; exactly what the right-hand group avoids.
  (modeline-segment :left "  %s" :face "comment")
  (modeline-segment :left "  %k" :face "constant" :bold t)

  ;; Right: what this buffer is, and where in it you are.
  ;; A mode's standing note, before the buffer's own facts: it is the only thing
  ;; on the right that is *transient*, and a segment that comes and goes is less
  ;; disturbing at the edge of the group than in the middle of it. Empty almost
  ;; always, and an empty `%N' drops the whole segment.
  (modeline-segment :right "%N  " :face "accent")
  (modeline-segment :right "%P  " :face "comment")
  (modeline-segment :right "%M  " :face "type")
  (modeline-segment :right "%n" :face "comment")
  (modeline-segment :right "%l:%c" :bold t)
  ;; The other bookend. Where you are in the file is the one fact on the right
  ;; that is about *reading* rather than about the buffer's identity, and a block
  ;; at the far edge gives the strip two ends instead of one — the mode at the
  ;; left, the position at the right, and everything that is merely information
  ;; laid out flat between them.
  (modeline-segment :right "  %p " :face "accent" :bold t :filled t)
  nil)

(default-modeline)

;;; ---------------------------------------------------------------------------
;;; ...and clicking one
;;;
;;; The renderer answers which *segment* the pointer landed on, as the template
;;; it was drawn from plus the text it expanded to, and calls this. Same
;;; division of labour as a scene's hit test and a terminal's: the gesture is
;;; the renderer's arithmetic, what it means is policy, and policy is here.
;;;
;;; The template and not the text is the identity, and it has to be: `●' and
;;; `◈' are two glyphs *this file* chose a page ago, and a segment expanding to
;;; `Rust' one frame and `Org' the next is the same segment. `" %+"' is what
;;; the config wrote down, so it is the only stable name the strip has.
;;;
;;; Every arm either says something or does the one obvious thing. Nothing here
;;; is destructive except `%+', which saves — and saving is exactly what the dot
;;; is telling you about.

(defun %modeline-code (template)
  "The first `%' code in TEMPLATE, as a character, or NIL for a literal.

`%%' is a literal per cent and is stepped over rather than answered, which is
the only subtlety: a segment spelled `100%%' has no code in it at all, and
reporting `%' as its code would make it click as whatever `%' comes to mean
next."
  (let ((n (length template)) (i 0))
    (loop while (< i (1- n))
          do (if (char= (char template i) #\%)
                 (let ((c (char template (1+ i))))
                   (if (char= c #\%) (incf i 2) (return c)))
                 (incf i)))))

(defun %modeline-click (template text)
  "Somebody clicked the modeline segment drawn from TEMPLATE, which said TEXT.

TEXT is passed rather than re-read because a click lands on the pane under the
pointer and that is not necessarily the focused one — the readers here all
answer about the *live* buffer, so anything the segment itself already knows is
better taken from what was drawn."
  (case (%modeline-code template)
    ;; The dot. It is there to tell you the buffer is not on disk; clicking it
    ;; is the shortest possible way to act on that.
    (#\+ (if (buffer-modified-p)
             (save-file)
             (message "nothing to save")))
    ;; ...and its neighbour, which is the opposite claim.
    (#\r (message "read-only — nothing typed into this buffer will land"))
    ;; The buffer's name is shortened to fit; its path is not.
    ((#\b #\f) (message (or (buffer-file-name) (buffer-name))))
    ;; The last message, clicked, is a request to see the ones before it.
    (#\s (messages-buffer))
    ;; The mode pill, and the two mode segments beside the position.
    (#\m (message (format nil "~:(~a~) state — Esc for normal, `i' to insert"
                          (evil-state))))
    ((#\M #\n)
     (let ((minor (minor-modes)))
       (message (format nil "~a~@[ + ~{~a~^ ~}~]" (major-mode) minor))))
    ;; Where you are, spelled out: the strip has room for `12:4' and not for
    ;; what it is 12 of.
    ((#\l #\c #\p)
     (message (format nil "line ~d of ~d, column ~d" (line-number) (line-count)
                      (1+ (column)))))
    (#\P (message (format nil "permissions ~a" text)))
    ;; A mode's standing note. Whoever put it there owns what it says, so the
    ;; hook is the answer rather than a table here — see `modeline-note'.
    (#\N (dolist (f *modeline-note-functions* (message text))
            (let ((said (ignore-errors (funcall f text))))
              (when said (return (message said))))))
    (t nil))
  nil)

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

(defparameter *leader-keys* (make-hash-table :test #'equal)
  "Command name -> the leader sequence that runs it, for the dashboard to show.
First binding wins: `switch-buffer' is on both `SPC b b' and `SPC j j', and the
one worth printing is the one in the group the command belongs to, which is the
one written first.")

(defun leader-key (command)
  "The leader sequence bound to COMMAND, or NIL. For display only — nothing
here makes the binding work, `define-key' does that."
  (gethash command *leader-keys*))

(defun define-leader (keys command)
  "Bind a SPC-prefixed sequence in the modes that have a leader."
  (unless (gethash command *leader-keys*)
    (setf (gethash command *leader-keys*) keys))
  (dolist (mode *leader-modes*) (define-key mode keys command)))

;;; ---------------------------------------------------------------------------
;;; A project you do not have yet
;;;
;;; The hole beside `SPC p o': that one browses for a project on the disk, and a
;;; repository you have never cloned is not on the disk. So this is the third
;;; door in — a URL — and it lands you in exactly the same place the other two
;;; do, a directory that `project-switch' will remember from now on.
;;;
;;; In Lisp and not in `crates/project', because none of it is a fast primitive:
;;; it is one `git clone', a name derived from a URL, and a `find-file'. The
;;; Rust side already knows what a project *is*; it does not need to learn git.
;;;
;;; `:wait nil' and a poll, which is the shape `math-code.lisp' established and
;;; for its reason: a clone is seconds to minutes, and the Lisp thread is where
;;; every keystroke's `after-change-hook' runs. Waiting for git here would stall
;;; completion and diagnostics for the length of the clone.

(defparameter *project-directory* "~/Code"
  "Where `project-clone' puts a repository. `~/' is expanded; the directory is
created if it is not there. Set it in your init to keep your checkouts
somewhere else.")

(defun %expand-home (path)
  "PATH with a leading `~/' replaced by the home directory."
  (if (and (>= (length path) 2) (string= "~/" path :end2 2))
      (namestring (merge-pathnames (subseq path 2) (user-homedir-pathname)))
      path))

(defun %project-repo-name (url)
  "The directory a clone of URL lands in: the last path segment, `.git' off.

Handles both spellings git accepts — `https://host/owner/repo.git' and
`git@host:owner/repo.git' — because the second's separator is a colon and the
first's is a slash, and taking the last of either is the whole difference."
  (let* ((trimmed (string-right-trim "/" url))
         (cut (position-if (lambda (c) (member c '(#\/ #\:))) trimmed :from-end t))
         (name (if cut (subseq trimmed (1+ cut)) trimmed))
         (dot (search ".git" name :from-end t)))
    (if (and dot (= dot (- (length name) 4))) (subseq name 0 dot) name)))

(defvar *project-clone* nil
  "The clone in flight, as a plist of :PROCESS :TARGET :URL, or NIL.
One at a time: two clones would want two messages and there is one echo area.")

(defun project-clone-poll ()
  "Notice that a clone has finished, and open what it produced.

On `*point-moved-functions*', for `math-code-build-poll''s reason: there is no
timer in this editor, and the moment the answer becomes interesting is the
moment you do something. Costs one read of a special variable per movement."
  (let ((clone *project-clone*))
    (when (and clone
               (handler-case
                   (not (eq :running (ext:external-process-wait
                                      (getf clone :process) nil)))
                 ;; A handle we can no longer ask about is a clone we can no
                 ;; longer follow. The directory test below is the real verdict.
                 (serious-condition () t)))
      (let ((target (getf clone :target)))
        (setf *project-clone* nil)
        ;; The filesystem and not the exit status, so a clone that half-failed
        ;; is not announced as a project: git leaves nothing behind when it
        ;; cannot fetch, and a directory that exists is one you can open.
        (if (probe-file (merge-pathnames ".git/" target))
            (progn (message (format nil "cloned into ~a" target))
                   (find-file target))
            (message (format nil "clone failed: ~a" (getf clone :url)))))))
  nil)

;;; `add-hook' and not a PUSHNEW: it binds the list if it is the first to mention
;;; it, and joining twice — which a config reload does — does not stack a second
;;; copy of the function on the hook.
(add-hook '*point-moved-functions* 'project-clone-poll)

(defun project-clone ()
  "Clone a git repository into `*project-directory*' and open it.

The third way into a project, beside `SPC p p' (one you have visited) and
`SPC p o' (one on the disk). Bound to `SPC p n'."
  (read-string "Git URL: "
    (lambda (url)
      (let ((url (and url (string-trim " " url))))
        (cond
          ((or (null url) (zerop (length url))))
          (*project-clone*
           (message (format nil "already cloning ~a" (getf *project-clone* :url))))
          (t
           (let* ((dir (%expand-home *project-directory*))
                  (target (merge-pathnames
                           (format nil "~a/" (%project-repo-name url))
                           (pathname (format nil "~a/" (string-right-trim "/" dir))))))
             (cond
               ((probe-file target)
                ;; Already here, which is not a failure — it is the answer to
                ;; the question you asked, one step early.
                (message (format nil "already cloned: ~a" target))
                (find-file target))
               (t
                (ensure-directories-exist target)
                (handler-case
                    ;; `:output nil' is the null device, so there is no pipe to
                    ;; fill and deadlock on — the same trade `math-code.lisp'
                    ;; makes, and it costs git's progress bar, which is not
                    ;; something a one-line echo area could have shown anyway.
                    (multiple-value-bind (stream code process)
                        (ext:run-program
                         "git" (list "clone" url (namestring target))
                         :input nil :output nil :error nil :wait nil)
                      (declare (ignore stream code))
                      (setf *project-clone*
                            (list :url url :target target :process process))
                      (message (format nil "cloning ~a into ~a…" url target)))
                  (serious-condition (e)
                    (message (format nil "cannot run git: ~a" e))))))))))))
  nil)

;;; ---------------------------------------------------------------------------
;;; The recipes in the Makefile
;;;
;;; `SPC p c' already runs *the* build — cargo build, npm run build, make — and
;;; one key for the usual thing is right. But a Makefile is a menu, and the entry
;;; you want is as often `test' or `fmt' or `deploy' as it is the default. So this
;;; reads the menu and asks.
;;;
;;; In Lisp for `project-clone''s reason: it is a line scanner, a picker and a
;;; shell command, and not one of the three is hot. Rust already knows how to run
;;; a program and how to draw a completing prompt; it does not need make's
;;; grammar as well.

(defun %directory-of (file)
  "The directory FILE sits in, as a pathname."
  (make-pathname :name nil :type nil :defaults file))

(defun %makefile-near (start)
  "The nearest Makefile at or above START, or NIL.

Climbs rather than asking for the project root, and that is the useful answer in
a monorepo: there is a Makefile per package, and the one you mean is the one
beside the file you are looking at, not the one at the top of the checkout.

Walks the *directory components* rather than repeatedly taking a pathname's
parent. Shortening a list is total — `(:absolute \"a\" \"b\")' to `(:absolute)'
and then to nothing — where climbing by pathname has to decide when it has
reached the root, and gets there by asking a question the root itself answers
badly."
  (let ((parts (pathname-directory (merge-pathnames start))))
    (loop for n from (length parts) downto 1
          for dir = (make-pathname :directory (subseq parts 0 n)
                                   :name nil :type nil)
          thereis (or (probe-file (merge-pathnames "Makefile" dir))
                      (probe-file (merge-pathnames "makefile" dir))))))

(defun %makefile-targets (path)
  "Every target PATH declares, in the order it declares them.

A line scanner rather than an understanding of make, deliberately. A rule starts
in column zero and carries a colon; a recipe line starts with a tab; everything
else is a comment, an assignment or a directive. That reads multi-target rules
correctly and is wrong only about computed names — and a `$(BINS):' names nothing
a picker could have offered anyway.

Targets beginning with a dot are skipped, which is how `.PHONY' and `.DEFAULT'
stay out of the list without needing to be known by name: the real targets a
`.PHONY' line mentions have rules of their own further down."
  (with-open-file (in path :if-does-not-exist nil)
    (when in
      (let ((found '()))
        (loop for line = (read-line in nil nil)
              while line
              do (let* ((colon (position #\: line))
                        ;; `:=', `::=' and `:::=' all assign. Skipping the run of
                        ;; colons and asking what follows reads every one of them
                        ;; — where a fixed-width window after the first colon
                        ;; reads `:=' and quietly lets `::=' through as a rule.
                        (after-colons
                         (and colon (position-if-not (lambda (c) (char= c #\:))
                                                     line :start colon))))
                   (when (and colon (plusp colon)
                              (not (find (char line 0) '(#\Tab #\Space #\#)))
                              (not (find #\= line :end colon))
                              (not (and after-colons (char= (char line after-colons) #\=)))
                              (not (find #\$ line :end colon))
                              (not (find #\% line :end colon)))
                     (dolist (name (split-string (subseq line 0 colon) #\Space))
                       (let ((name (string-trim '(#\Space #\Tab) name)))
                         (when (and (plusp (length name))
                                    (char/= (char name 0) #\.)
                                    (not (member name found :test #'string=)))
                           (push name found)))))))
        (nreverse found)))))

(defun project-make ()
  "Pick a target out of the nearest Makefile and run it in an output pane.

A child rather than `run-process', for the reason that function's own docstring
gives: a build is minutes, and `run-process' parks the Lisp thread for its whole
length. Through a PTY the output streams as it arrives and a failure is left on
screen where it can be read.

`output:' rather than `shell:' because a build is something you *read*. It opens
beside what you were working on, the editor keeps the keyboard — so the motions
and the searches all work while make is still printing — and `q' dismisses the
pane. Pressing the key again re-runs into the same one instead of stacking a
second."
  (let* ((here (let ((file (buffer-file-name)))
                 (if file
                     (%directory-of (pathname file))
                     *default-pathname-defaults*)))
         (makefile (%makefile-near here))
         (dir (and makefile (%directory-of makefile))))
    (cond
      ((null makefile) (message "no Makefile here or above"))
      (t
       (let ((targets (%makefile-targets makefile)))
         (if (null targets)
             (message (format nil "no targets in ~a" makefile))
             (completing-read
              "make: " targets
              (lambda (target)
                (when (and target (plusp (length target)))
                  (terminal (format nil "output:make:make -C ~a ~a"
                                    (namestring dir) target))))))))))
  nil)

;;; ---------------------------------------------------------------------------
;;; Clickable links in the terminal
;;;
;;; A click the child did not ask for used to do nothing at all — a shell never
;;; turns mouse reporting on — so the row it landed on comes here instead. What
;;; counts as a link is decided in Lisp rather than in Rust, because it is policy
;;; and the tables below are the hook a config changes it through.

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

;;; ---------------------------------------------------------------------------
;;; Editing the selection
;;;
;;; `region' answers a (BEG . END) of character offsets, or NIL when nothing is
;;; selected; `region-text' is the text between them.
;;;
;;; `replace-region' does the delete and the insert as *one* operation. Doing it
;;; as `delete-region' then `insert' would also work, but Lisp here runs
;;; alongside your typing rather than freezing the editor the way Emacs does, so
;;; a keystroke can land between two separate commands. One call cannot be
;;; interrupted; two can.

(defun surround-region (left right)
  "Wrap the selection in LEFT and RIGHT.

The generic one, and the shape `docs/reference.org' and the tutorial teach
`replace-region' with. Org's own emphasis keys do not go through it: emphasis
has to be trimmed to one line and taken back off again, which is
`%org-emphasize' below."
  (let ((r (region)))
    (if r
        (replace-region (car r) (cdr r)
                        (concatenate 'string left (region-text) right))
        (message "no selection"))))

(defun %org-emphasis-span (&optional word)
  "What emphasis should go around: (BEG . END), or NIL when there is nothing.

The selection when there is one — shrunk to what org will actually *render*,
which the raw selection frequently is not. Emphasis may not cross a line break
and does not render with whitespace against a marker, so Visual-Line, whose
region reaches the newline, used to put the closing marker on the *next line*,
and a Visual selection that caught the trailing space gave `* word *'. Both show
as literal asterisks. Shrinking rather than refusing: you pressed bold on a
line, and the line's text is what you meant by it.

With nothing selected and WORD, the run of non-whitespace point is in. `SPC m b'
is pressed in Normal, where by construction there is never a selection, so
without this the leader spelling could only ever answer `no selection'.

NIL when nothing is left of it, which is what selecting only whitespace is."
  (let ((r (region)))
    (when (or r word)
      (let* ((beg (if r (car r) (line-start)))
             (raw (if r (buffer-substring beg (cdr r)) (line-string)))
             ;; Only the first line of what was selected is a candidate at all.
             (text (subseq raw 0 (or (position #\Newline raw) (length raw))))
             (n (length text))
             ;; Point's offset into TEXT. `(- (point) (line-start))' rather than
             ;; `(column)', which is a screen column and counts a tab as several.
             (at (if r 0 (min (- (point) beg) n))))
        (flet ((blank (c) (member c '(#\Space #\Tab))))
          (let ((lo (if r
                        (or (position-if-not #'blank text) n)
                        (1+ (or (position-if #'blank text :end at :from-end t) -1))))
                (hi (if r
                        (1+ (or (position-if-not #'blank text :from-end t) -1))
                        (or (position-if #'blank text :start at) n))))
            (when (< lo hi)
              (cons (+ beg lo) (+ beg hi)))))))))

(defun %org-emphasize (mark &optional pair)
  "Put MARK around what is selected — or take it back off again.

Pressing bold on text that is already bold means *unbold it*: the alternative is
`**word**', which org renders as neither. The markers can be inside the span —
you selected `*word*' — or just outside it, which is what selecting or standing
in the `word' between them gives, and both count.

PAIR is what the typing chords pass, and it does two things. It opens an empty
pair when there is nothing to wrap, which is what makes `C-b' a key you press
*while typing* rather than after selecting: org's emphasis is a pair of
characters and the tedious part is always the closing one, which you type after
the word and then have to walk back over. `insert-at' puts both down in one edit
— one undo step, and no window for a keystroke to land between the two halves —
and point is then moved the one character inwards. It also reaches for no word:
mid-word, mid-sentence, the run you are standing in is not the one you meant.

PAIR is the org guard as well, and NIL outside org silently: `C-b' and `C-i' are
bound in the *insert* state rather than in org's keymap, because in Normal they
are already vim's page-up and jump-forward and org is not worth either. A state
binding is global, so this is what keeps them from putting a stray asterisk in a
`.rs' file. It costs nothing anywhere else — an unbound Ctrl chord in Insert
already does exactly nothing. `SPC m b' is in org's own keymap and needs no
guard, which is what lets `M-x org-bold' mean the same thing in a markdown
buffer."
  (unless (and pair (not (derived-mode-p 'org-mode)))
    (let ((visual (and (region) t))
          (span (%org-emphasis-span (not pair))))
      (cond
        (span
         (let* ((beg (car span))
                (end (cdr span))
                (text (buffer-substring beg end))
                (m (length mark)))
           (cond ((and (>= (length text) (* 2 m))
                       (string= mark text :end2 m)
                       (string= mark text :start2 (- (length text) m)))
                  (replace-region beg end (subseq text m (- (length text) m))))
                 ((and (string= mark (buffer-substring (- beg m) beg))
                       (string= mark (buffer-substring end (+ end m))))
                  (replace-region (- beg m) (+ end m) text))
                 (t
                  (replace-region beg end
                                  (concatenate 'string mark text mark))))
           ;; The selection it came from now covers text that has moved, and a
           ;; `d' pressed next would delete the wrong run. An operator leaves
           ;; Visual in vim, and this is one.
           (when visual (set-evil-state "normal"))))
        (pair
         (let ((at (point)))
           (insert-at at (concatenate 'string mark mark))
           (goto-char (+ at (length mark)))))
        (t (message "nothing to emphasise")))))
  nil)

;;; The three the leader spelling reaches, in whatever buffer you are in. With
;;; nothing selected they take the word point is in, which is the only thing
;;; `SPC m b' can mean: it is pressed in Normal, and Normal has no selection.
(defun org-bold () (%org-emphasize "*"))
(defun org-italic () (%org-emphasize "/"))
(defun org-code () (%org-emphasize "~"))

;;; ...and the three `C-b'/`C-i'/`C-~'-shaped ones, which are org's alone and
;;; open an empty pair instead of reaching for a word. Separate functions rather
;;; than a flag on the three above, because a key binding names a zero-argument
;;; function and both spellings have to be bindable.
(defun org-emphasis-bold () (%org-emphasize "*" t))
(defun org-emphasis-italic () (%org-emphasize "/" t))
(defun org-emphasis-code () (%org-emphasize "~" t))

;;; ---------------------------------------------------------------------------
;;; The rest of the runtime
;;;
;;; Everything below `library.lisp' in the shipped tree, loaded in one place so
;;; that a config need not name the files — which is what keeps a config written
;;; a year ago from missing the modes added since. A config that wants fewer can
;;; SETF the list before calling; one that wants its own file as well can PUSH
;;; onto the end of it.
;;;
;;; Order matters and each entry earns its place:
;;;
;;;   plugins/project.lisp
;;;                   wave 2 of the migration: the half of `crates/app/src/project.rs'
;;;                   that is policy. Where a project starts and what "build it"
;;;                   means there — `project-root', `project-dired',
;;;                   `project-compile', `project-test' — over two tables a config
;;;                   can PUSH a new kind of project onto. First, because it
;;;                   depends on nothing above `library.lisp' and the file finder
;;;                   it leaves in Rust depends on nothing here.
;;;
;;;   gui.lisp        the Lisp face of `crates/gui': `block', `text', `run',
;;;                   `image' and `rect' build a page, `scene-set' installs it on
;;;                   the live buffer, and a `:tag' makes a node clickable. Rust
;;;                   lays it out, wraps its text in a real font and routes a
;;;                   click back; *what* is on the page is entirely Lisp's. First
;;;                   because a mode that renders a document — `org-frozen-mode'
;;;                   — is a builder written on top of it. It takes the name
;;;                   `block' back from Common Lisp, which is the one symbol the
;;;                   runtime shadows; the file says why.
;;;
;;;   rpc.lisp        JSON-RPC over a child's stdin and stdout, knowing nothing
;;;                   about language servers.
;;;   lsp.lisp        the whole client written on top of it: the handshake,
;;;                   document synchronisation, go-to-definition, completion and
;;;                   diagnostics, all in Lisp. Rust owns the pipe, the framing
;;;                   and the process, and nothing else. Two servers ship —
;;;                   `pylsp' and `clangd' — and a third is one line in your
;;;                   init: `(lsp-register-server 'rust-mode "rust-analyzer")'.
;;;
;;;   which-key.lisp  what continues the prefix you just pressed, in the status
;;;                   line — and the same table read the other way round, as the
;;;                   docstring and key `M-x' now shows beside a command.
;;;   show-paren.lisp the matching delimiter, lit.
;;;   lisp-mode.lisp  one scanner for the shape of Lisp text, and the motion,
;;;                   kill, slurp/barf and indentation commands built on it.
;;;   repl.lisp       `C-c C-e' and friends, evaluating in *this* image and
;;;                   writing form and value into a transcript buffer.
;;;   parinfer.lisp   the inverse of that indenter — the indentation says where
;;;                   the closing parentheses go — on the same scanner.
;;;   org-latex.lisp  `$x^2$' as an image, on two primitives and an overlay.
;;;                   Before `org-modern.lisp', which calls its
;;;                   `org-latex-preview-new' from the org-mode body.
;;;   org-modern.lisp `display' overlays: heading stars become bullets, `[X]'
;;;                   becomes a tick, and `*bold*' shows its asterisks only
;;;                   while the cursor is in it. After `lsp.lisp' so it finds the
;;;                   `after-change-hook' that file installs.
;;;   org-fold.lisp   code folding's policy half: what an org subtree *is*, and
;;;                   the `z a' / `z M' / `z R' commands over the one thing Rust
;;;                   owns — an overlay carrying `fold' makes the lines after its
;;;                   first stop occupying rows. `*fold-subtree-functions*' is
;;;                   where another mode joins in.
;;;   org-structure.lisp
;;;                   the keys that edit the outline rather than the text in it:
;;;                   `M-RET' for another heading or item, `M-left'/`M-right' to
;;;                   promote and demote, `C-c C-t' for the TODO cycle, `C-c C-c'
;;;                   for a checkbox, `S-TAB' for the whole buffer.
;;;   org-frozen.lisp org as a *printed page*: `org-frozen-mode', which derives
;;;                   from `org-mode' and is read-only for real. Drawers,
;;;                   `#+keyword:' lines and block delimiters stop occupying
;;;                   rows; `#+TITLE:' is typeset as a title; a `#+begin_src'
;;;                   body gets a band, a gutter and *its own language's*
;;;                   highlighting; a table is drawn with aligned columns under a
;;;                   rule. `SPC m z' toggles it either way. After
;;;                   `org-fold.lisp' because it rebinds TAB over that file's
;;;                   `org-cycle', and before `math.lisp' because a curriculum is
;;;                   what it was built to display.
;;;   math.lisp       a whole maths curriculum as one org file — units, problems,
;;;                   and a place for your answer, all of it ordinary org with
;;;                   `#+ZEMACS_*' properties on the headings. The format is
;;;                   specified in `docs/curriculum.org'.
;;;   ai.lisp         coding agents — Claude Code, Cursor, opencode — as ordinary
;;;                   buffers, on the terminal the editor already has. `C-a' is
;;;                   the menu. The harness list is *data* in that file, so a
;;;                   fourth tool is one line and no Rust.
;;;   tutor.lisp      `SPC h t' — the tutorial, as a buffer that marks your
;;;                   homework rather than a page of prose that trusts you.
;;;                   After `repl.lisp' (`%eval-source') and `ai.lisp'
;;;                   (`executable-find').
;;;   math-code.lisp  the other half of a `programming' problem: its
;;;                   `#+begin_src python' block tangled to a file beside the
;;;                   curriculum, a venv built for it in the background, a window
;;;                   beside the question and one key that runs it in a terminal.
;;;                   Last, because it reads the schema from `math.lisp' and
;;;                   `executable-find' from `ai.lisp'.
;;;   math-written.lisp
;;;                   the other half of a `written' problem: a photograph of your
;;;                   handwriting, dropped in `~/Public/MathSync' by a phone,
;;;                   transcribed into org with LaTeX by a vision model and
;;;                   written into the Response of the problem point is inside.

(defparameter *runtime-modules*
  '("plugins/project.lisp"
    "gui.lisp" "rpc.lisp" "lsp.lisp"
    ;; Above `lsp.lisp' would be tidier and is wrong: `xref-show' is called from
    ;; there and `xref.lisp' calls nothing back, so the only ordering that
    ;; matters is that both are loaded before a key is pressed.
    "modes/xref.lisp"
    "modes/which-key.lisp" "modes/show-paren.lisp" "modes/avy.lisp"
    "modes/magit.lisp"
    "modes/lisp-mode.lisp" "modes/repl.lisp" "modes/parinfer.lisp"
    "modes/org-latex.lisp" "modes/org-modern.lisp" "modes/org-fold.lisp"
    ;; ...and `org-table.lisp' after both of them, because its three keys are
    ;; dispatchers that hand TAB, S-TAB and RET on to `org-cycle',
    ;; `org-global-cycle' and `org-open-at-point' when point is not in a table.
    ;; Loading it first would leave the bindings it displaces displaced.
    "modes/org-structure.lisp" "modes/org-table.lisp"
    ;; ...and the agenda after the table, because it is a scanner over
    ;; `xref-show' and reads `*org-todo-keywords*' out of `org-modern.lisp'.
    "modes/org-agenda.lisp" "modes/org-frozen.lisp"
    ;; After `org-modern.lisp', which is where `*org-mode-functions*' is
    ;; declared and therefore where the hook this adds itself to has to exist.
    "modes/mathsync.lisp"
    "modes/math.lisp" "modes/ai.lisp" "modes/tutor.lisp"
    "modes/math-code.lisp" "modes/math-written.lisp")
  "The shipped runtime, in load order. `modes/modes.lisp' is not here: it comes
up at the top of this file, because everything in it is needed before anything
else can be declared.")

(defun load-runtime-modules ()
  "LOAD every file in `*runtime-modules*'.

One `handler-case' per file rather than one around the lot, which is the whole
difference this function makes: a mode that fails to read used to take every
mode after it down with it, and the status line said only the first one's name.
Now the failure is named and the rest still load."
  (dolist (name *runtime-modules*)
    (let ((path (runtime-file name)))
      (when path
        (handler-case (load path :verbose nil :print nil)
          (error (e) (message (format nil "~a: not loaded — ~a" name e))))))))
