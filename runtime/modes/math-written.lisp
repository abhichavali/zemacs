;;;; math-written.lisp — a photograph of your handwriting becomes the answer.
;;;;
;;;; A written problem is worked on paper, because that is where mathematics is
;;;; done. What was missing was the last inch: getting the page back into the
;;;; document. So the phone drops a photograph into `~/Public/MathSync', a
;;;; watcher notices it, a vision model transcribes the handwriting into org
;;;; with LaTeX, and `math-response-insert' puts it under the problem's
;;;; `*** Response'. Nothing else about the curriculum changes: the file is
;;;; still plain org, and what lands in it is what you would have typed.
;;;;
;;;; `math.lisp' is part one and is the published contract this is written
;;;; against — `math-problem-at-point', `math-response-insert' and
;;;; `math-set-status'. None of it is reimplemented here. In particular
;;;; `math-response-insert' is already one `replace-region' and already
;;;; idempotent, which is exactly the property a capture pass needs, since the
;;;; same photograph may well be offered twice.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; WHICH PROBLEM — DECIDED BY POINT, AT THE MOMENT THE PHOTOGRAPH ARRIVES
;;;;
;;;; There is no arming step, no filename convention and no marker in the
;;;; document. The transcription goes to the written problem **point is inside
;;;; when the image lands**, and if point is not inside one the file is *left
;;;; alone* — not consumed, not moved, not sent anywhere — and the status line
;;;; says so. Move point into a problem and the next poll picks it up.
;;;;
;;;; Every alternative was worse. A filename convention (`u1p3.jpg') means
;;;; typing an identifier on a phone, which is the one input device where typing
;;;; an identifier is unbearable. An "arm the next photo" command means a mode
;;;; you can be in without knowing it, and a photograph that lands while you are
;;;; not armed is a photograph that goes somewhere arbitrary. Point is a thing
;;;; you can *see*, it is already where you are working, and getting it wrong
;;;; costs nothing because nothing is consumed.
;;;;
;;;; The binding is taken once, at arrival, and held as a **marker** rather than
;;;; an offset. That is doing two jobs at once: a marker slides with the text, so
;;;; the ten seconds a transcription takes are ten seconds you may spend editing
;;;; the document above the problem; and `marker-position' answers NIL while its
;;;; buffer is not the live one, so switching buffers mid-transcription is
;;;; refused rather than written into whatever happens to be on screen. Both are
;;;; the marker's own semantics — see `docs/reference.org' — and neither needed a
;;;; line of bookkeeping here.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; THE THREAD
;;;;
;;;; A watcher has to fire with nobody at the keyboard, so a hook cannot do it:
;;;; `after-change-hook' and `point-moved-hook' are both consequences of typing.
;;;; ECL has threads, and the loop below runs on one of its own.
;;;;
;;;; It calls editor primitives directly, and that is worth a paragraph because
;;;; it did not use to be possible. The host handle a primitive answers through
;;;; lives in Rust; it used to be a *thread-local*, installed on the thread ECL
;;;; was booted on, so `(message "x")' from a thread `mp:process-run-function'
;;;; made did nothing at all — no error, no message, no edit. It is now a
;;;; process-wide static (`crates/lisp/src/lib.rs'), which costs nothing — the
;;;; editor is behind a mutex either way, and the lock is still taken per
;;;; primitive — and turns a silent failure into working code.
;;;;
;;;; The rule that replaces it is `docs/threading.org''s own, unchanged: **a Lisp
;;;; function is not atomic, so a sequence that must be all-or-nothing is one
;;;; primitive**. Everything this file writes to a buffer goes through
;;;; `math-response-insert' and `math-set-status', and part one already made each
;;;; of those a single `replace-region' for exactly this reason. A watcher thread
;;;; is one more thing that can land between two keystrokes, which the editor has
;;;; been built to expect since the day Lisp got its own thread.
;;;;
;;;; What is *not* used here, and would be the obvious thing to reach for, is
;;;; `mp:interrupt-process' — running a closure in the context of the editor's
;;;; own Lisp thread. It works, until it does not: ECL services the interrupt
;;;; from a signal handler, `handle_all_queued_interrupt_safe' allocates on the
;;;; way in, and doing that while another thread has the collector busy is a
;;;; SIGBUS in `ecl_alloc_uncollectable'. It reproduced about one run in three
;;;; against a thread that base64s photographs. There is no `handler-case' for
;;;; it. Do not put it back.
;;;;
;;;; The other half of the rule is where the *slow* work goes, and it goes here,
;;;; on the watcher's thread: `curl', reading a three-megabyte photograph, the
;;;; base64. None of it may happen on the editor's Lisp thread, because a form
;;;; that does not return costs the image — every later form, every keybinding,
;;;; every mode hook queues behind it — and a hanging HTTP request is the exact
;;;; thing this file would otherwise introduce.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; WHAT IT COSTS WHEN NOTHING IS HAPPENING
;;;;
;;;; One `directory' call every `*math-written-interval*' seconds — three, by
;;;; default, which is nothing next to how long a phone takes to upload — and a
;;;; `file-length' per photograph found. **No editor lock is taken and no
;;;; redisplay work is done**, because with no candidate file there is nothing to
;;;; ask the editor about. The editor cannot tell the watcher is running. The
;;;; first time it touches the editor at all is when a file has appeared *and*
;;;; stopped growing, and that is one buffer scan.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; THE PHOTOGRAPH IS NOT DELETED. EVER.
;;;;
;;;; A captured photograph is *moved* to `done/' beside the watched directory.
;;;; Not deleted, and not merely remembered:
;;;;
;;;;   - deleting is unrecoverable, and the file may be the only copy of an
;;;;     afternoon's work. There is no case in which this program is right and
;;;;     the paper is gone.
;;;;   - remembering — a table of what has been seen — is lost on restart, and a
;;;;     restart would re-send every photograph in the directory to a paid API.
;;;;     Moving is the record, and it is a record `ls' can read.
;;;;
;;;; A file that could not be moved is remembered instead, and said once, which
;;;; is the degraded case rather than the design.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; ponytail: `%mw-' is this file's internal prefix — `math-written-' spelled
;;;; short, because it is on every second line. The public names are all spelled
;;;; out.
;;;;
;;;; ponytail: there is no transcoding. A HEIC straight off an iPhone is sent as
;;;; a HEIC, and a model that cannot read one answers an error which reaches the
;;;; status line. The ceiling is a phone set to "High Efficiency"; the upgrade is
;;;; one `sips -s format jpeg' on macOS, or the phone set to "Most Compatible",
;;;; which is where the fix belongs.
;;;;
;;;; ponytail: `*mw-sizes*' grows by one entry per photograph ever seen and is
;;;; never pruned. That is a few hundred short strings over a year of study. The
;;;; upgrade is dropping entries whose file has gone, which is a `remhash' in the
;;;; poll and is not worth writing until somebody has a table worth measuring.
;;;;
;;;; ponytail: the model is resolved once per *session*, not cached on disk, so
;;;; the first `SPC m w' of the day costs one extra GET. The ceiling is somebody
;;;; who restarts the editor forty times an hour; the upgrade is writing the
;;;; resolved slug into `~/.config/zemacs/', which is a file to invalidate and a
;;;; new way to be wrong about which model you are paying for. `(setf
;;;; *math-written-model* "...")' in your config is the same thing, said once,
;;;; where you can see it.
;;;;
;;;; ponytail: the key is re-read from the environment or the file on every
;;;; request rather than held in a variable, which costs one `open' and one `ls'
;;;; per photograph. That is nothing next to the round trip it precedes, and it
;;;; means rotating a key is editing a file rather than restarting an editor.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; What can be changed without editing code
;;;
;;; DEFPARAMETER for everything a user might set, DEFVAR for everything that is
;;; live state — the same split `rpc.lisp' makes, and for the same reason:
;;; `M-x reload-config' re-reads this file, and a running watcher that lost its
;;; own handle would be a thread nothing could stop.

(defparameter *math-written-directory*
  (merge-pathnames "Public/MathSync/" (user-homedir-pathname))
  "Where photographs of handwriting arrive. A pathname, or a string — absolute,
or relative to your home directory. Write it without a `~', which is a shell's
notation and not a pathname's.")

(defparameter *math-written-archive* "done/"
  "Where a captured photograph is moved to, relative to the watched directory.
Moved and never deleted: see the note at the top of this file.")

(defparameter *math-written-interval* 3
  "Seconds between polls of the watched directory. A phone takes longer than
this to upload a photograph, so anything smaller buys nothing; stopping the
watcher is felt within one interval, because that is how long it sleeps.")

(defparameter *math-written-extensions* '("jpg" "jpeg" "png" "heic" "heif" "webp")
  "File types treated as photographs. Matched case-insensitively, which matters:
a phone writes `IMG_0001.JPG'.")

(defparameter *math-written-key-variable* "OPENROUTER_API_KEY"
  "The environment variable read first for the OpenRouter key.")

(defparameter *math-written-key-file*
  (merge-pathnames ".config/zemacs/openrouter-key" (user-homedir-pathname))
  "A file holding the OpenRouter key on one line, read when the environment
variable is unset. Should be mode 600; a mode that is not gets one warning.")

(defparameter *math-written-endpoint*
  "https://openrouter.ai/api/v1/chat/completions"
  "Where a transcription is asked for.")

(defparameter *math-written-models-url* "https://openrouter.ai/api/v1/models"
  "The list `*math-written-model-wanted*' is resolved against.")

(defparameter *math-written-model-wanted* "GPT 5.6 luna"
  "The model, as a human says it. Resolved against OpenRouter's own list rather
than guessed at — see `math-written-model'.")

(defparameter *math-written-model* nil
  "The resolved OpenRouter slug, or NIL until one has been resolved. Set this
directly to override the lookup entirely:

  (setf *math-written-model* \"openai/gpt-4o\")")

(defparameter *math-written-timeout* 90
  "Seconds a transcription request may take. Handed to `curl --max-time', so it
is curl that gives up rather than us; the watcher thread has its own deadline ten
seconds later as a backstop.")

(defparameter *math-written-max-tokens* 4096
  "Ceiling on the transcription. A page of proof is well under this; a model with
no ceiling at all will happily narrate.")

(defparameter *math-written-autostart* nil
  "Whether opening a curriculum starts the watcher by itself.

NIL, deliberately. A background thread that posts your photographs to a paid API
is a thing to start on purpose, once, with `SPC m w'. Set it to T in your config
if you want opening the file to be the whole of the setup.")

;;; ---------------------------------------------------------------------------
;;; The prompt
;;;
;;; A deliverable in its own right, and the reason it is a DEFPARAMETER rather
;;; than a string literal buried in a function: it will be tuned against real
;;; handwriting, and tuning it must not mean editing code.
;;;
;;; Three things it is doing, in order of how often they go wrong. It forbids the
;;; wrappers a chat model reaches for by reflex — a preamble, a closing remark, a
;;; markdown fence — because every one of them ends up *in the document*. It
;;; pins the LaTeX dialect to org's (`$...$' and `\[ ... \]', never `$$'), which
;;; is what `org-latex-preview' typesets and what `docs/curriculum.org'
;;; specifies. And it insists on *fidelity over polish*, at length, because the
;;; failure that matters is not a wrong glyph — it is a model that quietly
;;; repairs your arithmetic, so that the page in the file is a proof you did not
;;; write and the mistake you needed to find is gone.

(defparameter *math-written-prompt*
  "Transcribe the handwritten mathematical solution in this photograph into org-mode.

Output the transcription and nothing else. No preamble, no closing remark, no
markdown code fences, no restatement of the question, and no org heading: never
begin a line with an asterisk, because this text is inserted under a heading
that already exists.

Mathematics:
- inline mathematics is $...$
- display mathematics is \\[ ... \\] on lines of its own
- never write $$ ... $$
- a multi-line derivation is \\begin{align} ... \\end{align} inside \\[ ... \\],
  aligned on & and broken with \\\\ ; amsmath and amssymb are available
- prose between the steps stays prose, on a line of its own

Fidelity, which matters more than polish:
- Transcribe what is on the paper. Do not correct arithmetic, do not tidy an
  argument, do not fill a gap, and do not finish an unfinished proof. If the
  writer is wrong, transcribe the wrong thing.
- Keep the order of the steps, the case splits, any numbering written beside
  them, and the writer's own words ('Suppose', 'By induction', 'hence', 'QED').
- A justification written beside a step goes inside \\text{...} within the
  display, or as prose on its own line beneath it.
- Omit crossed-out work.
- If a symbol is genuinely illegible, write \\text{[?]} rather than guessing.
- If the page holds no mathematics at all, output the single line: (illegible)"
  "What the vision model is asked to do. Sent as the text part of the message
that carries the photograph.")

(defparameter *math-written-illegible* "(illegible)"
  "The one answer that is not a transcription. A page the model could not read
must not mark a problem captured, so this is checked for rather than inserted.")

;;; ---------------------------------------------------------------------------
;;; The transport seam
;;;
;;; One function of one argument — a pathname — answering org text, or NIL when
;;; it could not. Everything above it is about *which problem*; everything below
;;; it is about HTTP. Replacing it is how the tests run without a network, and
;;; how somebody points this at a model on their own machine:
;;;
;;;   (setf *math-written-transport*
;;;         (lambda (path) (my-local-vision-model path)))

(defparameter *math-written-transport* '%mw-openrouter
  "How a photograph becomes org text: a function of one pathname, answering a
string or NIL. Anything `funcall' accepts.")

;;; ---------------------------------------------------------------------------
;;; Live state
;;;
;;; DEFVAR throughout, so `M-x reload-config' cannot orphan a running thread or
;;; forget which files have already been declined.

(defvar *mw-process* nil "The watcher thread, while there is one.")
(defvar *mw-watching* nil "Whether the watcher loop should keep going.")
(defvar *mw-busy* nil "Whether a capture is in flight.")

(defvar *mw-lock*
  #+threads (mp:make-lock :name "zemacs-math-written")
  #-threads nil
  "Held for the whole of one capture, so the watcher and `SPC m r' cannot
transcribe two photographs into the same problem at once.")

(defvar *mw-sizes* (make-hash-table :test #'equal)
  "Namestring -> the size it had at the previous poll. A file whose size has not
changed since then has finished uploading; one that is still growing has not.")

(defvar *mw-declined* (make-hash-table :test #'equal)
  "Namestrings we have already said `no written problem at point' about. The file
is still checked every poll — moving point is how you re-trigger — but the
message is given once, because a status line repeating itself every three seconds
is a status line you stop reading.")

(defvar *mw-skipped* (make-hash-table :test #'equal)
  "Namestrings the watcher will not try again. Set by a failure — a request that
did not come back, a transcription that would not land — because retrying a
failing request every three seconds is how a watcher spends somebody's money.
`math-capture-now' clears it: asking by hand is how you say you have fixed it.")

;;; ---------------------------------------------------------------------------
;;; Threads

(defun %mw-say (text)
  "Put TEXT in the status line and answer NIL.

The NIL is the point of the wrapper: nearly every failure below is `say why, and
answer nothing', so this is written once rather than as a `progn' per branch."
  (message text)
  nil)

(defun %mw-detached (name thunk)
  "Run THUNK off the editor's Lisp thread, and answer straight away.

Everything slow goes through here: `curl', a three-megabyte read, base64. On a
build of ECL without threads it runs inline and says so — the feature still
works, it just costs the image the time it takes, which is the honest degradation
rather than a silent one."
  #+threads
  (mp:process-run-function
   name
   (lambda ()
     (handler-case (funcall thunk)
       (error (e) (%mw-say (format nil "math: ~a" e))))))
  #-threads
  (progn
    (message "math: no threads in this ECL — the editor will wait for the model")
    (handler-case (funcall thunk)
      (error (e) (message (format nil "math: ~a" e))))))

(defmacro %mw-locked (&body body)
  "BODY with the capture lock held."
  #+threads `(mp:with-lock (*mw-lock*) ,@body)
  #-threads `(progn ,@body))

;;; ---------------------------------------------------------------------------
;;; Small text
;;;
;;; Four one-liners, written out rather than borrowed. `%math-words' in
;;; `math.lisp' is the closest thing to `%mw-tokens' and splits on the wrong
;;; characters for a model name; the rest have no equivalent anywhere.

(defun %mw-nonblank (s)
  "S trimmed, or NIL when there is nothing in it."
  (let ((trimmed (and s (string-trim '(#\Space #\Tab #\Return #\Newline) (string s)))))
    (when (and trimmed (plusp (length trimmed))) trimmed)))

(defun %mw-after (s char)
  "The part of S after the first CHAR, or all of S when there is none.
`openai/gpt-4o' -> `gpt-4o'; `OpenAI: GPT-4o' -> ` GPT-4o'."
  (let ((i (position char s)))
    (if i (subseq s (1+ i)) s)))

(defun %mw-normalise (s)
  "S reduced to its lowercase letters and digits.

`GPT 5.6 luna', `gpt-5.6-luna' and `GPT5.6Luna' all become `gpt56luna', which is
the whole of the matching rule: whatever a person types about a model, and
whatever a vendor punctuates it with, meet in the middle."
  (string-downcase (remove-if-not #'alphanumericp (string s))))

(defun %mw-tokens (s)
  "S split into its runs of letters and digits, lowercased.
`GPT 5.6 luna' -> (\"gpt\" \"5\" \"6\" \"luna\")."
  (let ((s (string-downcase (string s))) (out nil) (start nil))
    (dotimes (i (length s))
      (cond ((alphanumericp (char s i)) (unless start (setf start i)))
            (start (push (subseq s start i) out) (setf start nil))))
    (when start (push (subseq s start) out))
    (nreverse out)))

;;; ---------------------------------------------------------------------------
;;; Subprocesses
;;;
;;; The same shape `tutor.lisp' arrived at for running a child `ecl': drain the
;;; pipe as you go so a chatty child cannot fill it and deadlock, poll for the
;;; exit rather than blocking on it, and give up after a deadline, because ECL
;;; has no timeout of its own.

(defun %mw-run (program args &key stdin (timeout 30))
  "Run PROGRAM with ARGS. Answers (values OUTPUT STATUS), where STATUS is
:EXITED, :TIMEOUT or :BROKEN and OUTPUT is everything the child said.

STDIN, when given, is written to the child and the pipe is then *closed* — which
is the whole point of it here, because `curl --config -' reads until end of file
and would otherwise wait for one forever.

stderr is merged into stdout. That would corrupt a JSON reply if curl ever wrote
to both, and it does not: `--silent' takes the progress meter away and
`--show-error' leaves only the failures, which arrive *instead of* a body and are
worth far more in the status line than `no reply' would be."
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
;;; The key
;;;
;;; Two places, in order, and **neither of them is a file in this repository**.
;;; The environment first, because that is where a key belongs on a machine that
;;; has a secret manager; a file second, because that is where a key belongs on a
;;; machine that does not.
;;;
;;; The key never reaches a command line. `curl' gets it on *stdin*, as a config
;;; file — `header = "Authorization: Bearer ..."' — which is a form curl
;;; documents precisely so that `ps' cannot read your credentials off the process
;;; table. It is also never put in a message: every failure below names the
;;; *place* the key came from, never the key.

(defun %mw-mode-string (path)
  "PATH's permissions as `ls' spells them — `-rw-------' — or NIL.

`ls' rather than a stat: ECL can *set* a mode (`ext:chmod') and cannot read one,
and the first field of `ls -l' is specified by POSIX and identical on macOS and
Linux. The trailing `@' or `+' an extended attribute adds is dropped by taking
ten characters."
  (multiple-value-bind (out status) (%mw-run "ls" (list "-l" (namestring path)) :timeout 5)
    (when (and (eq status :exited) out)
      (let* ((line (subseq out 0 (or (position #\Newline out) (length out))))
             (field (subseq line 0 (or (position #\Space line) (length line)))))
        (when (>= (length field) 10) (subseq field 0 10))))))

(defun %mw-key-from-file ()
  "The key in `*math-written-key-file*', or NIL.

The permission warning is a warning and not a refusal: a key you can read is a
key that works, and refusing to use it would leave somebody with a broken editor
and a puzzle. Saying it once, with the mode in the message, is the whole of it."
  (let ((path (ignore-errors (probe-file *math-written-key-file*))))
    (when path
      (let ((mode (%mw-mode-string path)))
        (when (and mode (string/= "-rw-------" mode))
          (%mw-say (format nil "math: ~a is ~a — chmod 600 it"
                           (namestring path) mode))))
      (handler-case
          (%mw-nonblank (with-open-file (in path :external-format :utf-8)
                          (read-line in nil "")))
        (error () nil)))))

(defun %mw-key ()
  "The OpenRouter key, or NIL — having said where to put one."
  (or (%mw-nonblank (ext:getenv *math-written-key-variable*))
      (%mw-key-from-file)
      (%mw-say (format nil "math: no OpenRouter key — set $~a, or put one line in ~a (chmod 600)"
                       *math-written-key-variable*
                       (namestring *math-written-key-file*)))))

;;; ---------------------------------------------------------------------------
;;; JSON
;;;
;;; A reader and a string escaper, in ordinary Lisp.
;;;
;;; `rpc.lisp' looks like it has both halves and has neither. Its *decoding*
;;; happens in Rust, on the way in from a language server: a message arrives at
;;; `%rpc-event' already read, and there is no function anywhere that turns a
;;; string of JSON into a value. Its encoder is real, and taking it would mean
;;; depending on a file loaded inside a `handler-case' next to the language
;;; server machinery, for twelve lines of escaping. So: seventy lines, once.
;;;
;;; The mapping is deliberately the same one `rpc.lisp' documents, so that a
;;; reply reads the same whichever way it arrived: an object is an alist, an
;;; array is a list, `true' is T, and `false', `null', `{}' and `[]' are all NIL.

(defun %mw-json-string (s)
  "S as a JSON string literal, quotes and escapes included.
Non-ASCII passes through as itself: the body is written to a UTF-8 file, and
UTF-8 is what JSON is."
  (with-output-to-string (out)
    (write-char #\" out)
    (loop for c across (string s)
          do (cond ((char= c #\") (write-string "\\\"" out))
                   ((char= c #\\) (write-string "\\\\" out))
                   ((char= c #\Newline) (write-string "\\n" out))
                   ((char= c #\Return) (write-string "\\r" out))
                   ((char= c #\Tab) (write-string "\\t" out))
                   ((< (char-code c) 32) (format out "\\u~4,'0x" (char-code c)))
                   (t (write-char c out))))
    (write-char #\" out)))

(defun %mw-json (text)
  "TEXT decoded. Signals on malformed input; every caller is inside a
`handler-case', because a reply that is not JSON is a thing that happens (a proxy
login page, an HTML error) and must cost a message rather than the watcher."
  (let ((i 0) (n (length text)))
    (labels
        ((oops (why) (error "bad JSON at ~d: ~a" i why))
         (peek () (if (< i n) (char text i) (oops "end of input")))
         (eat (c) (unless (char= (peek) c) (oops c)) (incf i))
         (white ()
           (loop while (and (< i n)
                            (member (char text i) '(#\Space #\Tab #\Newline #\Return)))
                 do (incf i)))
         (word (s)
           (when (and (<= (+ i (length s)) n)
                      (string= s text :start2 i :end2 (+ i (length s))))
             (incf i (length s))
             t))
         (hex4 () (prog1 (parse-integer text :start i :end (+ i 4) :radix 16) (incf i 4)))
         (jstring ()
           (eat #\")
           (with-output-to-string (out)
             (loop
               (let ((c (peek)))
                 (incf i)
                 (cond
                   ((char= c #\") (return))
                   ((char/= c #\\) (write-char c out))
                   (t (let ((e (peek)))
                        (incf i)
                        (case e
                          (#\n (write-char #\Newline out))
                          (#\t (write-char #\Tab out))
                          (#\r (write-char #\Return out))
                          (#\b (write-char #\Backspace out))
                          (#\f (write-char #\Page out))
                          (#\u
                           ;; A surrogate pair is two escapes naming one
                           ;; character, and a model that emits an emoji in a
                           ;; transcription would otherwise arrive as two
                           ;; unpaired halves that no encoder can write.
                           (let ((cp (hex4)))
                             (when (and (<= #xD800 cp #xDBFF) (word "\\u"))
                               (let ((lo (hex4)))
                                 (setf cp (+ #x10000 (ash (- cp #xD800) 10)
                                             (- lo #xDC00)))))
                             (write-char (code-char cp) out)))
                          (t (write-char e out))))))))))
         (jnumber ()
           (let ((start i))
             (loop while (and (< i n) (find (char text i) "+-.eE0123456789"))
                   do (incf i))
             (when (= start i) (oops "a value"))
             (let ((*read-default-float-format* 'double-float))
               (values (read-from-string text nil nil :start start :end i)))))
         (jobject ()
           (white)
           (if (char= (peek) #\})
               (progn (incf i) nil)
               (loop collect (progn (white)
                                    (let ((k (jstring)))
                                      (white) (eat #\:)
                                      (cons k (value))))
                     do (white)
                        (case (peek)
                          (#\, (incf i))
                          (#\} (incf i) (loop-finish))
                          (t (oops "`,' or `}'"))))))
         (jarray ()
           (white)
           (if (char= (peek) #\])
               (progn (incf i) nil)
               (loop collect (value)
                     do (white)
                        (case (peek)
                          (#\, (incf i))
                          (#\] (incf i) (loop-finish))
                          (t (oops "`,' or `]'"))))))
         (value ()
           (white)
           (let ((c (peek)))
             (cond ((char= c #\{) (incf i) (jobject))
                   ((char= c #\[) (incf i) (jarray))
                   ((char= c #\") (jstring))
                   ((word "true") t)
                   ((word "false") nil)
                   ((word "null") nil)
                   (t (jnumber))))))
      (value))))

(defun %mw-jget (object &rest keys)
  "Walk a decoded reply: (%mw-jget r \"choices\" 0 \"message\" \"content\").

A string key indexes an object, an integer an array. Never signals — a reply
shaped differently from what you expected answers NIL, which is the convention
every reader in this editor follows."
  (dolist (k keys object)
    (setf object
          (cond ((integerp k) (and (listp object) (nth k object)))
                (t (and (consp object) (cdr (assoc k object :test #'equal))))))))

;;; ---------------------------------------------------------------------------
;;; The photograph
;;;
;;; Read as bytes and base64'd straight onto the output stream. Not into a
;;; string first: a three-megabyte photograph is a four-megabyte base64 string,
;;; and the request it goes into would then be a second copy of it. Writing as we
;;; encode costs one line and holds one copy.

(defparameter *mw-base64-alphabet*
  "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/")

(defun %mw-base64-onto (stream bytes)
  "Write BYTES to STREAM as base64. No line breaks: a data URL has none."
  (let ((n (length bytes)))
    (loop for i from 0 below n by 3
          do (let* ((b0 (aref bytes i))
                    (b1 (if (< (+ i 1) n) (aref bytes (+ i 1)) 0))
                    (b2 (if (< (+ i 2) n) (aref bytes (+ i 2)) 0))
                    (word (logior (ash b0 16) (ash b1 8) b2)))
               (write-char (char *mw-base64-alphabet* (ldb (byte 6 18) word)) stream)
               (write-char (char *mw-base64-alphabet* (ldb (byte 6 12) word)) stream)
               (write-char (if (< (+ i 1) n)
                               (char *mw-base64-alphabet* (ldb (byte 6 6) word))
                               #\=)
                           stream)
               (write-char (if (< (+ i 2) n)
                               (char *mw-base64-alphabet* (ldb (byte 6 0) word))
                               #\=)
                           stream)))))

(defun %mw-bytes (path)
  "PATH's contents as a byte vector, or NIL."
  (handler-case
      (with-open-file (in path :element-type '(unsigned-byte 8))
        (let ((bytes (make-array (file-length in) :element-type '(unsigned-byte 8))))
          (read-sequence bytes in)
          bytes))
    (error () nil)))

(defun %mw-mime (path)
  "The media type a data URL should claim for PATH."
  (let ((type (string-downcase (or (pathname-type path) "jpeg"))))
    (cond ((string= type "png") "image/png")
          ((string= type "webp") "image/webp")
          ((string= type "heic") "image/heic")
          ((string= type "heif") "image/heif")
          (t "image/jpeg"))))

;;; ---------------------------------------------------------------------------
;;; HTTP
;;;
;;; `curl' and `ext:run-program', because that is what this editor has instead of
;;; an HTTP stack, and it has managed without one so far. Two things are worth
;;; reading twice.
;;;
;;; The key goes down **stdin**, as a curl config file, and never onto the
;;; command line. The request body goes into a **temporary file**, because it
;;; carries a base64 photograph and there is only one stdin. That file holds no
;;; secret — it holds a picture that is already sitting in `~/Public' — so it is
;;; written plainly and deleted in an `unwind-protect' rather than fussed over.
;;;
;;; The HTTP status arrives appended to the body by `--write-out', on a line of
;;; its own. Without it a 401 with an empty body is indistinguishable from a
;;; network that swallowed the reply, and `HTTP 401' in the status line is the
;;; difference between `my key is wrong' and `the editor is broken'.

(defun %mw-curl-config (key)
  "The curl config file handed to it on stdin. One line, and the only place the
key appears in this program's memory outside the variable holding it."
  (format nil "header = \"Authorization: Bearer ~a\"~%"
          ;; A key with a quote or a backslash in it is not a key anybody has,
          ;; but a config file that broke on one would fail in a way nobody could
          ;; read.
          (with-output-to-string (out)
            (loop for c across key
                  do (when (or (char= c #\") (char= c #\\)) (write-char #\\ out))
                     (write-char c out)))))

(defun %mw-curl (url key &optional body-file)
  "Ask URL, with KEY as the bearer token. Answers (values BODY CODE), where CODE
is the HTTP status as an integer or NIL when there was not one.

With BODY-FILE it is a POST of that file as JSON; without, a GET."
  (unless (or (not (fboundp 'executable-find)) (executable-find "curl"))
    (return-from %mw-curl (values (%mw-say "math: no curl on $PATH") nil)))
  (let ((args (append (list "--silent" "--show-error" "--location"
                            "--max-time" (princ-to-string *math-written-timeout*)
                            "--write-out"
                            (concatenate 'string (string #\Newline) "%{http_code}")
                            "--config" "-")
                      (when body-file
                        (list "--request" "POST"
                              "--header" "Content-Type: application/json"
                              "--data-binary"
                              (concatenate 'string "@" (namestring body-file))))
                      (list url))))
    (multiple-value-bind (out status)
        (%mw-run "curl" args
                 :stdin (%mw-curl-config key)
                 ;; Ten seconds past curl's own deadline: curl is meant to be the
                 ;; one that gives up, and this is only here for the case where
                 ;; it does not.
                 :timeout (+ *math-written-timeout* 10))
      (case status
        (:timeout (values (%mw-say "math: the model did not answer in time") nil))
        (:broken (values (%mw-say (format nil "math: cannot run curl — ~a" out)) nil))
        (t (let* ((text (or out ""))
                  (nl (position #\Newline text :from-end t))
                  (tail (and nl (%mw-nonblank (subseq text (1+ nl)))))
                  (code (and tail (every #'digit-char-p tail) (parse-integer tail))))
             (if code
                 (values (subseq text 0 nl) code)
                 (values text nil))))))))

;;; ---------------------------------------------------------------------------
;;; The model
;;;
;;; `GPT 5.6 luna' is what the user said. It is *not* a slug, and guessing one
;;; would mean posting a photograph — and a bill — at a name nobody has
;;; published. So the list is fetched and the name resolved against it, and when
;;; nothing matches the answer is to say so and stop, rather than to fall back on
;;; a model the user did not choose.
;;;
;;; Matching is on the *normalised* form — letters and digits only, lowercased —
;;; against four spellings of each model: its id, its id without the vendor
;;; prefix, its name, and its name after the vendor's colon. That is what makes
;;; `GPT 5.6 luna', `gpt-5.6-luna' and `openai/gpt-5.6-luna' the same question.
;;;
;;; Only vision-capable models are considered, and that is a refusal rather than
;;; a preference: an exact match on a text-only model is a request that is certain
;;; to fail, and failing at the point of *choosing* is much easier to understand
;;; than failing later with a photograph attached.

(defun %mw-vision-p (model)
  "Whether MODEL takes an image.

Two spellings, because OpenRouter has published both: `input_modalities' as a
list, and the older `modality' as `text+image->text'."
  (let* ((arch (%mw-jget model "architecture"))
         (inputs (%mw-jget arch "input_modalities"))
         (modality (or (%mw-jget arch "modality") (%mw-jget model "modality"))))
    (and (or (and (listp inputs) (member "image" inputs :test #'equal))
             (and (stringp modality) (search "image" modality)))
         t)))

(defun %mw-model-names (model)
  "Every spelling of MODEL worth matching a human's words against, normalised."
  (let ((id (%mw-jget model "id"))
        (name (%mw-jget model "name")))
    (remove nil
            (list (and (stringp id) (%mw-normalise id))
                  (and (stringp id) (%mw-normalise (%mw-after id #\/)))
                  (and (stringp name) (%mw-normalise name))
                  (and (stringp name) (%mw-normalise (%mw-after name #\:)))))))

(defun %mw-suggestions (vision wanted)
  "A few vision-capable ids to offer when WANTED matched nothing.

Ranked by how many of WANTED's words appear in a model's names, so asking for
`GPT 5.6 luna' is answered with the GPT models rather than with whatever happens
to be first in the list."
  (let* ((tokens (%mw-tokens wanted))
         (scored (sort (mapcar (lambda (m)
                                 (cons (count-if
                                        (lambda (tok)
                                          (some (lambda (k) (search tok k))
                                                (%mw-model-names m)))
                                        tokens)
                                       m))
                               vision)
                       #'> :key #'car))
         (best (or (remove-if (lambda (pair) (zerop (car pair))) scored) scored)))
    (mapcar (lambda (pair) (%mw-jget (cdr pair) "id"))
            (subseq best 0 (min 5 (length best))))))

(defun %mw-pick-model (models wanted)
  "The slug in MODELS that WANTED names, or NIL.
Second value: a few vision-capable ids to offer instead.

Exact first, and a *unique* substring match second — `luna' naming the only
vision model with `luna' in it is an answer; `gpt' naming forty of them is not,
and answering the first would be picking for somebody."
  (let* ((want (%mw-normalise wanted))
         (vision (remove-if-not #'%mw-vision-p models))
         (exact (find-if (lambda (m) (member want (%mw-model-names m) :test #'string=))
                         vision))
         (partial (unless exact
                    (remove-if-not (lambda (m)
                                     (some (lambda (k) (search want k))
                                           (%mw-model-names m)))
                                   vision))))
    (cond (exact (values (%mw-jget exact "id") nil))
          ((and partial (null (rest partial)))
           (values (%mw-jget (first partial) "id") nil))
          (t (values nil (%mw-suggestions (or partial vision) wanted))))))

(defun %mw-resolve-model ()
  "Fetch OpenRouter's model list and resolve `*math-written-model-wanted*'
against it. Answers the slug, or NIL having said why. Runs off the Lisp thread."
  (let ((key (%mw-key)))
    (when key
      (multiple-value-bind (body code) (%mw-curl *math-written-models-url* key)
        (let ((models (and body
                           (handler-case (%mw-jget (%mw-json body) "data")
                             (error () nil)))))
          (cond
            ((null models)
             (%mw-say (format nil "math: cannot read OpenRouter's model list~@[ (HTTP ~a)~]"
                              code))
             nil)
            (t
             (multiple-value-bind (slug suggestions)
                 (%mw-pick-model models *math-written-model-wanted*)
               (cond
                 (slug (setf *math-written-model* slug)
                       (%mw-say (format nil "math: ~a is ~a" *math-written-model-wanted* slug))
                       slug)
                 (t
                  ;; Left unset rather than pointed somewhere plausible. Posting
                  ;; to the wrong model is a wrong answer you pay for.
                  (setf *math-written-model* nil)
                  (%mw-say
                   (format nil "math: no vision model is called ~s — try ~{~a~^, ~}, then (setf *math-written-model* ...)"
                           *math-written-model-wanted* suggestions))
                  nil))))))))))

(defun math-written-model ()
  "Resolve `*math-written-model-wanted*' against OpenRouter's model list, and say
what it resolved to. Off the Lisp thread, because it is a network call — so this
answers immediately and the *answer* arrives in the status line a moment later."
  (%mw-detached "zemacs-math-model" #'%mw-resolve-model)
  (message (format nil "math: asking OpenRouter about ~s" *math-written-model-wanted*)))

;;; ---------------------------------------------------------------------------
;;; The transcription

(defun %mw-scratch-file ()
  "Where the request body is built. Named after the process, so two zemacs
running at once do not write over each other's."
  (merge-pathnames (format nil "zemacs-math-~d.json" (ext:getpid))
                   (or (ignore-errors
                        (let ((tmp (ext:getenv "TMPDIR")))
                          (when (%mw-nonblank tmp)
                            (pathname (if (char= #\/ (char tmp (1- (length tmp))))
                                          tmp
                                          (concatenate 'string tmp "/"))))))
                       #p"/tmp/")))

(defun %mw-write-body (model path out)
  "Write the chat-completions request for PATH to the stream OUT.

Built with `format' rather than with `json-encode' because the photograph is
poured through it: encoding first would mean a four-megabyte string existing only
to be written out and dropped. The three values that vary are escaped properly;
everything else here is a literal.

No `temperature'. It is the obvious thing to pin at 0 for a transcription and
several current models reject the parameter outright, and a request refused for
a setting we did not need is a bad trade."
  (let ((bytes (%mw-bytes path)))
    (when bytes
      (format out "{\"model\":~a,\"max_tokens\":~d,\"messages\":[{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":~a},{\"type\":\"image_url\",\"image_url\":{\"url\":\"data:~a;base64,"
              (%mw-json-string model) *math-written-max-tokens*
              (%mw-json-string *math-written-prompt*) (%mw-mime path))
      (%mw-base64-onto out bytes)
      (write-string "\"}}]}]}" out)
      t)))

(defun %mw-unfence (text)
  "TEXT with a markdown code fence taken off it.

The prompt forbids one. Models emit one anyway, and three lines of defence here
are cheaper than a curriculum with ``` in it."
  (let* ((s (string-trim '(#\Space #\Tab #\Return #\Newline) text))
         (nl (position #\Newline s)))
    (if (and (>= (length s) 3) (string= "```" s :end2 3) nl)
        (let* ((rest (string-right-trim '(#\Space #\Tab #\Return #\Newline)
                                        (subseq s (1+ nl))))
               (close (search "```" rest :from-end t)))
          (if close
              (string-right-trim '(#\Space #\Tab #\Return #\Newline)
                                 (subseq rest 0 close))
              rest))
        s)))

(defun %mw-content (reply)
  "The assistant's text in a decoded REPLY, or NIL.

`content' is a string from most models and a list of parts from some, and both
shapes are read rather than one being called wrong."
  (let ((content (%mw-jget reply "choices" 0 "message" "content")))
    (cond ((stringp content) content)
          ((listp content)
           (%mw-nonblank
            (with-output-to-string (out)
              (dolist (part content)
                (let ((text (%mw-jget part "text")))
                  (when (stringp text) (write-string text out)))))))
          (t nil))))

(defun %mw-openrouter (path)
  "Transcribe the photograph at PATH through OpenRouter. Answers org text, or NIL
having said why. The default `*math-written-transport*'; runs off the Lisp
thread."
  (let ((key (%mw-key))
        (model (or *math-written-model* (%mw-resolve-model))))
    (when (and key model)
      (let ((body (%mw-scratch-file)))
        (unwind-protect
             (when (handler-case
                       (with-open-file (out body :direction :output
                                                 :if-exists :supersede
                                                 :if-does-not-exist :create
                                                 :external-format :utf-8)
                         (%mw-write-body model path out))
                     (error (e)
                       (%mw-say (format nil "math: cannot read ~a — ~a"
                                        (file-namestring path) e))
                       nil))
               (multiple-value-bind (text code) (%mw-curl *math-written-endpoint* key body)
                 (let* ((reply (and text (handler-case (%mw-json text) (error () nil))))
                        (refusal (%mw-jget reply "error" "message"))
                        (content (%mw-content reply)))
                   (cond
                     ((null text) nil)     ; %mw-curl has already said why
                     (refusal (%mw-say (format nil "math: the model refused — ~a" refusal)))
                     ((null reply)
                      (%mw-say (format nil "math: unreadable reply~@[ (HTTP ~a)~] — ~a"
                                       code
                                       (subseq text 0 (min 120 (length text))))))
                     (content content)
                     (t (%mw-say (format nil "math: the model said nothing~@[ (HTTP ~a)~]"
                                         code)))))))
          (ignore-errors (delete-file body)))))))

(defun %mw-transcribe (path)
  "PATH as org text, through whatever `*math-written-transport*' is.

The tidying is here rather than in the transport so that it applies to all of
them: a markdown fence taken off, and *nothing at all* rather than an empty
string — a model that answered with whitespace must not blank a Response that
already had an answer in it."
  (let ((text (funcall *math-written-transport* path)))
    (and text (%mw-nonblank (%mw-unfence text)))))

;;; ---------------------------------------------------------------------------
;;; Binding and landing
;;;
;;; The two halves that touch the editor, and the only two. Each is a buffer scan
;;; and at most two edits, and each of those edits is one primitive — see the
;;; note at the top of this file about what a thread here is allowed to assume.

(defun %mw-binding ()
  "Which problem a transcription arriving *now* belongs to.

Answers (:marker M :where TEXT), or NIL when point is not inside a written
problem — in a programming problem, in the instruction prose, in the Contents, or
in a buffer that is not a curriculum at all.

The marker is made with the advancing insertion type, so that text typed
immediately before the problem's first star pushes it along rather than being
swallowed by it."
  (let ((problem (and (math-curriculum-p) (math-problem-at-point))))
    (when (and problem (string= (getf problem :kind) "written"))
      (list :marker (make-marker (getf problem :begin) t)
            :where (format nil "~@[~a · ~]~a" (getf problem :id) (buffer-name))))))

(defun %mw-release (binding)
  "Drop BINDING's marker. Every path out of a capture goes through here or
through `%mw-land', because a marker nobody holds is still adjusted on every
edit."
  (when binding
    (delete-marker (getf binding :marker))))

(defun %mw-land (binding text)
  "Put TEXT in the Response of the problem BINDING names, and mark it captured.
Answers T, or NIL having said why.

`marker-position' is the whole verification. It answers NIL when the marker's
buffer is not the live one and NIL when the text it named has been deleted, which
are exactly the two ways the ten seconds a transcription takes can invalidate a
binding — so one reader covers `you switched buffers' and `you deleted the
problem' without either being special-cased.

The status is set *after* the response, and set to `done' meaning **captured**.
Nothing here has an opinion about whether the mathematics is right; `SPC m t'
sets it straight back. See the refusal at the top of `math.lisp'."
  (let ((marker (getf binding :marker)))
    (unwind-protect
         (let* ((at (marker-position marker))
                (problem (and at (find at (math-problems)
                                       :key (lambda (p) (getf p :begin))))))
           (cond
             ((null problem)
              (message "math: that problem has moved, or is in another buffer — photograph kept")
              nil)
             ((null (math-response-insert problem text)) nil)
             (t (math-set-status problem *math-done*) t)))
      (delete-marker marker))))

;;; ---------------------------------------------------------------------------
;;; The directory

(defun %mw-directory ()
  "`*math-written-directory*' as a directory pathname."
  (let ((d *math-written-directory*))
    (if (pathnamep d)
        d
        (merge-pathnames (if (and (plusp (length d)) (char= #\/ (char d (1- (length d)))))
                             d
                             (concatenate 'string d "/"))
                         (user-homedir-pathname)))))

(defun %mw-photograph-p (path)
  (and (pathname-name path)
       (stringp (pathname-type path))
       (member (pathname-type path) *math-written-extensions* :test #'string-equal)))

(defun %mw-images ()
  "Every photograph in the watched directory, oldest first.

Oldest first because the watcher takes one per poll and the one that has been
waiting longest is the one you meant. `math-capture-now' takes the last."
  (handler-case
      (sort (remove-if-not #'%mw-photograph-p
                           (directory (merge-pathnames "*.*" (%mw-directory))))
            #'< :key (lambda (p) (or (ignore-errors (file-write-date p)) 0)))
    (error () nil)))

(defun %mw-size (path)
  (handler-case (with-open-file (in path :element-type '(unsigned-byte 8))
                  (file-length in))
    (error () nil)))

(defun %mw-settled-p (path)
  "Whether PATH has stopped growing — true when it is the same size now as it
was at the previous poll.

A photograph is *being uploaded* when it appears, and a file half-written is a
file a model reads as corrupt. Two polls at the same size is the whole test: it
costs one `file-length', it needs no notification API, and being wrong once means
waiting three more seconds. A size of zero is never settled, because a file that
has been created but not yet written has a stable size too."
  (let* ((key (namestring path))
         (now (%mw-size path))
         (before (gethash key *mw-sizes*)))
    (setf (gethash key *mw-sizes*) now)
    (and now (plusp now) (eql now before))))

(defun %mw-archive (path)
  "Move PATH into the `done/' subdirectory. Answers where it went, or NIL.

A name already taken gets a counter rather than being overwritten: two
photographs of two problems can perfectly well be called `IMG_0001.jpg' on two
days, and losing the older one to the newer would be the same mistake as deleting
it."
  (handler-case
      (let* ((dir (merge-pathnames *math-written-archive* (%mw-directory)))
             (name (pathname-name path))
             (type (pathname-type path)))
        (ensure-directories-exist dir)
        (let ((target (loop for n from 0
                            for candidate = (merge-pathnames
                                             (make-pathname
                                              :name (if (zerop n)
                                                        name
                                                        (format nil "~a-~d" name n))
                                              :type type)
                                             dir)
                            unless (probe-file candidate) return candidate)))
          (rename-file path target)
          target))
    (error (e)
      ;; The photograph stays where it is and is remembered instead, which is the
      ;; degraded case: a restart will offer it again, and `math-response-insert'
      ;; being idempotent is what makes that survivable rather than duplicated.
      (setf (gethash (namestring path) *mw-skipped*) t)
      (%mw-say (format nil "math: transcribed, but could not move ~a — ~a"
                       (file-namestring path) e))
      nil)))

;;; ---------------------------------------------------------------------------
;;; One capture
;;;
;;; The pipeline, and the only place the work is written down in order: read
;;; which problem (a buffer scan), transcribe (a network round trip), write it
;;; back (two atomic edits), move the file. All of it on the calling thread —
;;; which is the watcher's, or `math-capture-now''s one-shot.

(defun %mw-capture (path &optional (binding :ask))
  "Transcribe PATH into the problem BINDING names. Answers T when it landed.

BINDING defaults to reading one *now*, which is what the watcher wants: the
photograph has just arrived, so `now' is the moment it arrived.
`math-capture-now' passes one it read before it spawned anything instead, so that
the binding is where point was when the key was pressed."
  (let ((key (namestring path))
        (name (file-namestring path)))
    (%mw-locked
      (setf *mw-busy* t)
      (unwind-protect
           (let ((binding (if (eq binding :ask) (%mw-binding) binding)))
             (cond
               ((null binding)
                ;; Not consumed, not sent, not moved. Said once — the file is
                ;; re-examined every poll, so moving point *is* the retry.
                (unless (gethash key *mw-declined*)
                  (setf (gethash key *mw-declined*) t)
                  (%mw-say (format nil "math: ~a is waiting — put point in a written problem"
                                   name)))
                nil)
               (t
                (remhash key *mw-declined*)
                (%mw-say (format nil "math: transcribing ~a into ~a"
                                 name (getf binding :where)))
                (let ((text (%mw-transcribe path)))
                  (cond
                    ((null text)
                     ;; Whatever failed has already said so. Not retried: a
                     ;; failing request re-sent every three seconds is how a
                     ;; watcher spends somebody's money.
                     (%mw-release binding)
                     (setf (gethash key *mw-skipped*) t)
                     nil)
                    ((string= text *math-written-illegible*)
                     (%mw-release binding)
                     (setf (gethash key *mw-skipped*) t)
                     (%mw-say (format nil "math: ~a could not be read — kept, and the problem left alone"
                                      name))
                     nil)
                    ((%mw-land binding text)
                     (%mw-archive path)
                     (%mw-say (format nil "math: ~a captured from ~a" (getf binding :where) name))
                     t)
                    (t
                     (setf (gethash key *mw-skipped*) t)
                     nil))))))
        (setf *mw-busy* nil)))))

;;; ---------------------------------------------------------------------------
;;; The watcher

(defun %mw-poll ()
  "One turn: pick the oldest settled photograph nobody has given up on, and
capture it.

One per poll, on purpose. Two photographs captured in the same three seconds
would both be bound to whatever problem point is in, and the second would replace
the first — so the queue drains at one every interval, which is also the rate at
which a person moves point."
  (let ((ready (remove-if-not #'%mw-settled-p
                              (remove-if (lambda (p) (gethash (namestring p) *mw-skipped*))
                                         (%mw-images)))))
    (when ready (%mw-capture (first ready)))))

(defun %mw-loop ()
  "The watcher thread. Resolves the model first — there is no point watching for
photographs there is nowhere to send — and then polls until told to stop."
  (unwind-protect
       (cond
         ((not (or *math-written-model* (%mw-resolve-model)))
          (%mw-say "math: watcher off — no model"))
         (t
          (loop while *mw-watching*
                do (handler-case (%mw-poll)
                     (error (e) (%mw-say (format nil "math: watcher — ~a" e))))
                   ;; Floored, because an interval of zero in somebody's config
                   ;; is a background thread spinning a core for the rest of the
                   ;; session, and there is no reading of "watch this directory"
                   ;; that wants it.
                   (sleep (max 0.05 *math-written-interval*)))))
    (setf *mw-watching* nil
          *mw-process* nil)))

(defun %mw-watching-p ()
  "Whether there is a live watcher. Both halves, because the flag alone would
still read true after a thread that died of something this file did not expect."
  (and *mw-watching* *mw-process*
       #+threads (mp:process-active-p *mw-process*)
       #-threads nil))

(defun math-watch-start ()
  "Watch `*math-written-directory*' for photographs of handwriting.

Refuses, with a reason, rather than starting a thread that cannot work: no key,
no directory, and no model are each a sentence in the status line and a watcher
that stays off."
  (cond
    ((%mw-watching-p)
     (message (format nil "math: already watching ~a" (namestring (%mw-directory)))))
    #-threads
    ;; ponytail: no fallback poller. A watcher is *the* thing that needs a
    ;; thread, and pretending to have one by polling from a hook would fire only
    ;; while you type — which is exactly the case it exists to cover. `SPC m r'
    ;; is the whole of this feature on such a build.
    (t (message "math: no threads in this ECL — SPC m r captures one by hand"))
    #+threads
    ((null (%mw-key)) nil)
    #+threads
    ((null (ignore-errors (probe-file (%mw-directory))))
     (message (format nil "math: no directory ~a to watch" (namestring (%mw-directory)))))
    #+threads
    (t
     (setf *mw-watching* t)
     (setf *mw-process* (mp:process-run-function "zemacs-math-watch" #'%mw-loop))
     (message (format nil "math: watching ~a every ~a s"
                      (namestring (%mw-directory)) *math-written-interval*)))))

(defun math-watch-stop ()
  "Stop watching. Felt within one interval — the thread is asked to stop rather
than killed, so that a transcription already in flight finishes and lands."
  (cond ((not *mw-watching*) (message "math: not watching"))
        (t (setf *mw-watching* nil)
           (message (format nil "math: watching stops within ~a s"
                            *math-written-interval*)))))

(defun math-watch ()
  "Turn the photograph watcher on or off. `SPC m w'."
  (if (%mw-watching-p) (math-watch-stop) (math-watch-start)))

(defun math-capture-now ()
  "Transcribe the newest photograph in `*math-written-directory*' into the
written problem point is in. `SPC m r'.

The binding is read here, before anything is spawned — so it is where point was
when you pressed the key, and a photograph with nowhere to go is refused
immediately rather than after a network round trip. Clears whatever the watcher
had given up on: asking by hand is how you say you have fixed it."
  (let ((path (first (last (%mw-images))))
        (binding (%mw-binding)))
    (cond
      ((null path)
       (%mw-release binding)
       (message (format nil "math: no photograph in ~a" (namestring (%mw-directory)))))
      ((null binding)
       (message "math: point is not in a written problem — SPC m n finds one"))
      (*mw-busy*
       (%mw-release binding)
       (message "math: a capture is already running"))
      (t
       (remhash (namestring path) *mw-skipped*)
       (remhash (namestring path) *mw-declined*)
       ;; Said *before* the thread starts, not after: the worker's own first line
       ;; is "transcribing ...", and racing it would leave the older sentence on
       ;; screen.
       (message (format nil "math: reading ~a" (file-namestring path)))
       (%mw-detached "zemacs-math-capture" (lambda () (%mw-capture path binding)))
       nil))))

;;; ---------------------------------------------------------------------------
;;; Keys
;;;
;;; In the `math-curriculum' minor mode beside part one's motions, so they exist
;;; in a curriculum and nowhere else. `SPC m r' and `SPC m w' are the two that
;;; were free: `b i c' are org's emphasis commands, `l L' the LaTeX preview,
;;; `m M a f F o' org-modern's, and `n p t g s' part one's. Shadowing any of them
;;; inside a curriculum would mean the one org buffer where org's own keys do
;;; something else.

(define-key "math-curriculum" "SPC m r" "math-capture-now")
(define-key "math-curriculum" "SPC m w" "math-watch")

;;; `math-written-model' is deliberately not bound. It is a setup step you run
;;; once, or after changing `*math-written-model-wanted*', and `M-x' is what an
;;; occasional verb is for.

;;; Autostart, off by default, on the same hook `math.lisp' uses to switch the
;;; minor mode on. PUSHNEW and a DEFVAR first for the same reason it does: a
;;; config reload must not double the list, and a build that never loaded
;;; `math.lisp' must get an empty list rather than an unbound variable.
(defvar *org-mode-functions* nil)

(defun math-written-maybe-watch ()
  "Start the watcher when a curriculum opens, if the config asked for that."
  (when (and *math-written-autostart*
             (not (%mw-watching-p))
             (ignore-errors (math-curriculum-p)))
    (math-watch-start)))

(pushnew 'math-written-maybe-watch *org-mode-functions*)

;;; A hook, not a verb: running it from `M-x' does nothing you can see, and
;;; `*hidden-commands*' is `init.lisp''s existing answer for exactly that. The
;;; rest of this file is either a `%'-prefixed helper, which `refresh-commands'
;;; already skips, or a command worth having in the list.
(when (boundp '*hidden-commands*)
  (pushnew "math-written-maybe-watch" *hidden-commands* :test #'string=))
