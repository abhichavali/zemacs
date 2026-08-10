;;;; LSP — the eglot equivalent, written in Lisp on top of `rpc.lisp'.
;;;;
;;;; Nothing in Rust knows what a language server is. This file is the whole
;;;; client: the handshake, which server serves which mode, what to do with a
;;;; reply, where a diagnostic goes. That is deliberate — it is the layer a
;;;; config wants to bend, and the image is where bending costs nothing.
;;;;
;;;; *Adding a language server is one line here and nothing anywhere else:*
;;;;
;;;;   (lsp-register-server 'go-mode "gopls")
;;;;   (lsp-register-server 'rust-mode "rust-analyzer")
;;;;   (lsp-register-server 'c-mode "clangd" "--background-index")
;;;;
;;;; What is bound to what:
;;;;
;;;;   (lsp)                      start a server for the live buffer
;;;;   (lsp-stop) (lsp-restart)
;;;;   (lsp-goto-definition)      jump to the definition under the cursor
;;;;   (lsp-diagnostics-at-point) echo the diagnostic on this line
;;;;   (lsp-list-diagnostics)     every diagnostic, in a buffer
;;;;   (lsp-status)               what is running
;;;;   (lsp-diagnostics &optional path)   the data, for anything that draws it
;;;;   (lsp-complete)             ask for completions here, and pop them up
;;;;   (lsp-complete-next) (lsp-complete-previous)
;;;;   (lsp-complete-accept) (lsp-complete-abort)
;;;;
;;;; ---------------------------------------------------------------------------
;;;; What the editor tells us, and what it costs
;;;;
;;;; One signal: `after-change-hook', which the application fires whenever the
;;;; document's revision moves. It carries no *delta*, so this file keeps a
;;;; **shadow copy** of every open document — the text the server was last sent —
;;;; and `textDocument/didChange' names the one run that differs between the
;;;; shadow and the buffer, as a range in the shadow's own coordinates. A server
;;;; that offers only full sync still gets the whole document; that negotiation
;;;; is in `%lsp-initialized', and getting it backwards is the expensive mistake,
;;;; because a full-sync server handed a range takes the range's *text* as the
;;;; entire file and the damage surfaces much later as nonsense completions.
;;;;
;;;; The shadow is exactly what the server has *by construction*: it is written
;;;; in the same breath as the notification carrying it, and never from anywhere
;;;; else. So the one text this client can send wrongly — the buffer moving
;;;; between the two reads in `lsp-ensure' — repairs itself on the next keystroke
;;;; rather than desynchronising the session, which is the failure incremental
;;;; sync is otherwise famous for.
;;;;
;;;; ponytail: the buffer is still *read* whole every keystroke.
;;;; `after-edit-hook' carries (START OLD-END NEW-END TEXT) and would save that
;;;; read, and it is deliberately not used: the image evaluates it a turn or more
;;;; later (`docs/threading.org') and the record names no *buffer*, so a delta
;;;; produced while you were in one file arrives while you are in another and
;;;; nothing on this side can tell — `(buffer-file-name)' is as late as the hook
;;;; is. Applying it to the wrong shadow is precisely the corruption the
;;;; paragraph above exists to rule out, so the read stays. Ceiling: one
;;;; `buffer-string' per revision, which is a memcpy and a READ and is not felt
;;;; below a few hundred KB — the whole *document* no longer goes down the pipe,
;;;; which was the cost that mattered. Upgrade path: the buffer's path in the
;;;; record, one argument in `after_edit_form' in `crates/app/src/main.rs'.
;;;;
;;;; The escaping of what does go out is the one thing that is *not* in Lisp
;;;; (`%json-quote'), because doing it a character at a time in the image on
;;;; every keystroke is exactly the case the boundary rule exists for.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; After-change
;;;
;;; `*after-change-functions*' and the `after-change-hook' the application calls
;;; by name used to be declared here, because the LSP client was the only thing
;;; that wanted them. They are in `modes.lisp' now, beside `point-moved-hook'
;;; and for the reason the note there gives — five other files wanted them too,
;;; and a hook is not the property of whichever feature asked first. Nothing
;;; about this client changed: it registers with `add-hook' like everybody else.

;;; ---------------------------------------------------------------------------
;;; Paths and URIs
;;;
;;; **The one place in the image that still counts bytes**, and it has to: a
;;; percent-escape names a byte, so `é' in a path is `%C3%A9' and not `%E9'.
;;; Everything else — the buffer, a form arriving from a keybinding, a server's
;;; reply — is characters; see the model note at the top of
;;; `crates/lisp/src/shim.c'. `ext:string-to-octets' and `ext:octets-to-string'
;;; are the codec, because ECL ships one and a hand-rolled pair here would be a
;;; third copy of arithmetic the shim already does correctly.

(defun %uri-unreserved-p (code)
  "True for the bytes a `file:' URI may carry literally: ASCII letters and
digits, `-._~' and the separator itself."
  (or (<= 65 code 90) (<= 97 code 122) (<= 48 code 57)
      (member code '(45 46 95 126 47))))

(defun lsp-path-uri (path)
  "PATH as a `file:' URI, percent-encoded."
  (with-output-to-string (out)
    (write-string "file://" out)
    (loop for byte across (ext:string-to-octets (string path)
                                                :external-format :utf-8)
          do (if (%uri-unreserved-p byte)
                 (write-char (code-char byte) out)
                 (format out "%~2,'0X" byte)))))

(defun lsp-uri-path (uri)
  "The filesystem path inside a `file:' URI, or NIL for anything else — a
server is allowed to answer with a location in a jar, and jumping to one is not
something this editor can do.

The escapes are gathered as *bytes* and decoded in one go at the end, which is
the only order that works: `%C3%A9' is two escapes and one character, so
decoding each escape where it stands would answer with the Latin-1 reading of
its own encoding — the mojibake this whole boundary exists to prevent. A
character sitting in the URI unescaped, which is malformed and does happen,
contributes its own UTF-8 and so survives the round trip too."
  (when (and (stringp uri) (>= (length uri) 7) (string= "file://" uri :end2 7))
    (let* ((raw (subseq uri 7))
           (n (length raw))
           (out (make-array n :element-type '(unsigned-byte 8)
                              :adjustable t :fill-pointer 0))
           (i 0))
      (loop while (< i n)
            do (let ((ch (char raw i)))
                 (cond ((and (char= ch #\%) (<= (+ i 3) n)
                             (let ((v (ignore-errors
                                       (parse-integer raw :start (1+ i)
                                                          :end (+ i 3)
                                                          :radix 16))))
                               (when v (vector-push-extend v out) (incf i 3)))))
                       (t (loop for b across (ext:string-to-octets
                                              (string ch) :external-format :utf-8)
                                do (vector-push-extend b out))
                          (incf i)))))
      (ext:octets-to-string (coerce out '(vector (unsigned-byte 8)))
                            :external-format :utf-8))))

(defun %file-directory (path)
  "PATH's directory, trailing slash included."
  (let ((i (position #\/ path :from-end t)))
    (if i (subseq path 0 (1+ i)) "./")))

(defun %parent-directory (dir)
  "DIR's parent, or NIL at the filesystem root. DIR ends in a slash."
  (let ((i (position #\/ dir :from-end t :end (max 0 (1- (length dir))))))
    (when i (subseq dir 0 (1+ i)))))

(defparameter *lsp-root-markers*
  '(".git" "compile_commands.json" "CMakeLists.txt" "pyproject.toml"
    "setup.py" "setup.cfg" "Cargo.toml" "go.mod" "package.json")
  "Files that mean \"the project starts here\". A VCS root is first because it
is the answer that is right when a repository contains several build files.")

(defun lsp-project-root (path)
  "The directory a server should be started in for PATH.

Walks up looking for a marker and falls back to the file's own directory, which
is what a scratch file outside any project wants."
  (or (loop for dir = (%file-directory path) then (%parent-directory dir)
            while dir
            when (some (lambda (m)
                         (ignore-errors
                          (probe-file (concatenate 'string dir m))))
                       *lsp-root-markers*)
              return dir)
      (%file-directory path)))

(defvar *lsp-roots* (make-hash-table :test #'equal)
  "PATH -> its project root, remembered.

Not an optimisation for its own sake: `lsp-ensure' runs on every keystroke and
`lsp-project-root' walks the filesystem, so without this every character typed
would `probe-file' its way to the root of the disk. A project that grows a
`.git' while the editor is open keeps the root it had — `M-x lsp-restart' after
`git init' is the whole workaround.")

(defun %lsp-root-for (path)
  (or (gethash path *lsp-roots*)
      (setf (gethash path *lsp-roots*) (lsp-project-root path))))

;;; ---------------------------------------------------------------------------
;;; Which server for which mode
;;;
;;; The whole registry, and the whole reason a third language is a one-line
;;; change. DEFVAR so a reload does not drop what a config registered before
;;; this file was read; the two shipped entries below are re-registered anyway.

(defvar *lsp-servers* (make-hash-table :test #'equal)
  "Major mode name -> a plist of :PROGRAM and :ARGS.")

;;; A mode is a string to the editor and a symbol to whoever is writing the
;;; config. Spelled out here rather than borrowing `%mode-name' from
;;; `modes.lisp': this file needs nothing else from the mode system, and being
;;; loadable on its own is worth two lines.
(defun %lsp-mode-name (mode) (string-downcase (string mode)))

(defun lsp-register-server (mode program &rest args)
  "Use PROGRAM as the language server for MODE."
  (setf (gethash (%lsp-mode-name mode) *lsp-servers*)
        (list :program (string program) :args args))
  (%lsp-mode-name mode))

;;; Two arguments, asked one after the other — the case that made a collector
;;; worth writing rather than nesting the callbacks by hand. Spelled as a call to
;;; `%interactive' behind an `fboundp' guard rather than as the `interactive'
;;; macro, because this file is loaded on its own by the LSP tests and by anyone
;;; who wants the client without the standard library; an undefined macro at the
;;; top level would take the rest of the file with it. Same escape hatch the math
;;; modes use for `*hidden-commands*'.
(when (fboundp '%interactive)
  (funcall '%interactive 'lsp-register-server
           ;; The mode names we already know a language id for. Not a closed set
           ;; — nothing is required to match, so registering for a mode that is
           ;; not in the table is still a matter of typing it.
           (list (cons "Mode: " (lambda () (mapcar #'car *lsp-language-ids*)))
                 (cons "Program: " :string))))

(defvar *lsp-language-ids*
  '(("python-mode" . "python") ("c-mode" . "c") ("rust-mode" . "rust")
    ("javascript-mode" . "javascript") ("json-mode" . "json")
    ("toml-mode" . "toml") ("lisp-mode" . "commonlisp"))
  "Major mode name -> the `languageId' a server expects. A mode that is not
here sends its own name with `-mode' cut off, which is right often enough that
the table only holds the exceptions.")

(defun lsp-language-id (mode)
  (or (cdr (assoc mode *lsp-language-ids* :test #'string=))
      (let ((i (search "-mode" mode :from-end t)))
        (if i (subseq mode 0 i) mode))))

;;; ---------------------------------------------------------------------------
;;; Columns
;;;
;;; Two units meet in this file and only one of them is the editor's. LSP counts
;;; a column in **UTF-16 code units**; the editor counts *characters*
;;; (`(point)', `(column)'), and so does the text, everywhere, in both languages.
;;; The two agree exactly across the whole BMP — `café' is four of each — so an
;;; accent never moves a column.
;;;
;;; What moves one is a character **outside** the BMP: an emoji is one character
;;; and a *surrogate pair*, so a string literal with three of them in it put
;;; every position later on that line three columns to the left, and a jump asked
;;; about the wrong symbol or about none.
;;;
;;; There was a third unit until recently — buffer text arrived as UTF-8 bytes,
;;; one Lisp character per byte — and it is worth knowing it is gone, because
;;; half the arithmetic below existed only to undo it: `%lsp-char-start' snapped
;;; a `mismatch' back off the middle of an `é', and every scan here stepped over
;;; continuation bytes. The shim decodes now (`crates/lisp/src/shim.c'), the
;;; editor's offsets and Lisp's indices are the same integers, and what is left
;;; is the one conversion LSP actually asks for.
;;;
;;; Both directions are needed and both are here. Outgoing is a character column
;;; on the line point is on; incoming is a code-unit column on a line of a file
;;; that is very often *not* the buffer on screen, which is what the shadow copy
;;; below is measured against — the same string the server is talking about.
;;;
;;; In Lisp and not as a reader in `query.rs', which was the upgrade path written
;;; down for this and turns out to be the wrong half: a `didChange' range is a
;;; position in the document the server *has*, not the one the editor has, and
;;; core cannot see that string. A reader would have covered point and left the
;;; ranges needing this code anyway, and one conversion written twice is how the
;;; highlight ends up disagreeing with the jump.

(defun %lsp-utf16-length (text &key (start 0) (end (length text)))
  "How many UTF-16 code units TEXT[START:END) is.

Not simply `(- end start)' because of the astral plane: a character at #x10000
or above — an emoji — is spelled by UTF-16 as a *surrogate pair* and counts
twice. Everything else counts once."
  (loop for i from start below end
        sum (if (>= (char-code (char text i)) #x10000) 2 1)))

(defun %lsp-utf16-column (text chars)
  "The UTF-16 column CHARS characters into the line TEXT."
  (%lsp-utf16-length text :end chars))

(defun %lsp-char-column (text units)
  "The character column UNITS UTF-16 code units into the line TEXT — the
inverse of `%lsp-utf16-column'.

Stops at the end of the line rather than running past it: a server may name a
column one past the last character, and every column past *that* is the same
answer. A unit landing inside a surrogate pair rounds up to the whole character,
which is the only answer an editor with no half-characters can give."
  (let ((chars 0) (u 0) (n (length text)))
    (loop while (and (< chars n) (< u units))
          do (incf u (if (>= (char-code (char text chars)) #x10000) 2 1))
             (incf chars))
    chars))

(defun %lsp-point-position ()
  "Point as an LSP `Position'. LSP counts lines from 0 and `line-number' from 1."
  (jobj "line" (1- (line-number))
        "character" (%lsp-utf16-column (line-string) (column))))

(defun %lsp-position-in (text at)
  "The LSP `Position' of the character offset AT in TEXT, a whole document."
  (let ((bol (let ((nl (position #\Newline text :end at :from-end t)))
               (if nl (1+ nl) 0))))
    (jobj "line" (count #\Newline text :end at)
          "character" (%lsp-utf16-length text :start bol :end at))))

;;; ---------------------------------------------------------------------------
;;; Sessions
;;;
;;; One per (mode, project root) pair, which is eglot's rule and the right one:
;;; two Python projects open at once want two `pylsp's, and two files in the
;;; same project want one.

(defvar *lsp-sessions* (make-hash-table :test #'equal)
  "KEY -> a plist: :conn :mode :root :state :queue :opened :versions :sync.
:state is :STARTING until `initialized' has gone out, then :READY. :sync is what
the server said about `didChange' and is NIL — meaning full text, the safe half
— until it has said it.")

(defvar *lsp-conn-keys* (make-hash-table)
  "CONN -> KEY, so an incoming message can find its session.")

(defvar *lsp-shadow* (make-hash-table :test #'equal)
  "PATH -> the document the server has, character for character.

Written only where a notification carrying that text is sent, which is what makes
it true rather than hopeful: not a cache of the buffer, a record of what went
down the pipe. It is what a `didChange' range is measured against and what an
incoming diagnostic's column is measured against — see \"Document
synchronisation\" for both. Keyed by path and not per session because
`lsp-ensure' only ever synchronises the *live* buffer, so one path is one
server's document at a time; it lives up here because `%lsp-forget' drops it
along with the rest of a session's state.")

(defun %lsp-key (mode root) (concatenate 'string mode " " root))
(defun %lsp-get (key prop) (getf (gethash key *lsp-sessions*) prop))
(defun %lsp-set (key prop value) (setf (getf (gethash key *lsp-sessions*) prop) value))

(defun lsp-session-for-buffer ()
  "The session KEY serving the live buffer, or NIL. Does not start one."
  (let ((path (buffer-file-name))
        (mode (major-mode)))
    (when (and path (gethash mode *lsp-servers*))
      (let ((key (%lsp-key mode (%lsp-root-for path))))
        (when (gethash key *lsp-sessions*) key)))))

;;; The client's half of the handshake. Deliberately small: everything declared
;;; here is something this editor can actually do, and a capability you claim
;;; and do not honour is a server sending you work you throw away.
;;;
;;; No `positionEncoding' either, and that is now a claim rather than a gap: the
;;; protocol's default is UTF-16 and every column crossing this file is
;;; converted to it — see "Columns", below. The only other spelling every server
;;; offers is `utf-8', which in LSP means *bytes*, so negotiating would trade one
;;; conversion for another and buy nothing.
(defun %lsp-capabilities ()
  (jobj "general" (jobj "markdown" (jobj "parser" "none"))
        "textDocument"
        (jobj "synchronization" (jobj "didSave" t "willSave" :false)
              "definition" (jobj "linkSupport" t)
              ;; Completion, and most of what is in here is a *refusal*, which
              ;; is the useful half of this negotiation: a server told we do
              ;; snippets sends `$1' placeholders we would insert literally, and
              ;; one told we resolve lazily sends items with no `detail' until
              ;; asked. Both are real features and neither is built, so both are
              ;; declined and the server sends us plain text it has finished
              ;; filling in.
              ;;
              ;; `resolveSupport' is declined by being **absent**, and that is
              ;; not a style choice. It is the one field here that is an *object*
              ;; in the protocol (`{"properties": [...]}'), so a `false' is not a
              ;; refusal but a type error — and a strict server refuses the whole
              ;; handshake over it rather than the one capability. rust-analyzer
              ;; does exactly that: "invalid type: boolean `false`, expected
              ;; struct CompletionItemCapabilityResolveSupport", and then exits,
              ;; which is why registering it used to buy you a dead session and
              ;; no completions at all. Absent already means unsupported.
              ;;
              ;; `documentationFormat' is the one claim that is now a *request*:
              ;; there is a panel beside the popup to put a docstring in, so ask
              ;; for one. Plaintext first, because that is the order of
              ;; preference and nothing here renders markdown — a server with
              ;; only markdown sends it anyway and we show the source, which for
              ;; a signature and a paragraph is very nearly the same text.
              "completion"
              (jobj "completionItem"
                    (jobj "snippetSupport" :false
                          "insertReplaceSupport" :false
                          "documentationFormat" (jarr "plaintext" "markdown"))
                    "contextSupport" t)
              "publishDiagnostics" (jobj "relatedInformation" :false))
        "workspace" (jobj "workspaceFolders" :false
                          "configuration" :false)))

(defun %lsp-start (mode root)
  "Spawn the server for MODE at ROOT and begin the handshake. Answers the
session KEY, or NIL if the program could not be started."
  (let* ((spec (gethash mode *lsp-servers*))
         (key (%lsp-key mode root))
         (conn (and spec
                    (rpc-start (getf spec :program)
                               :args (getf spec :args)
                               :cwd root
                               :name (getf spec :program)
                               :on-notify #'%lsp-notification
                               :on-request #'%lsp-request
                               :on-exit #'%lsp-exit))))
    (when conn
      (setf (gethash key *lsp-sessions*)
            (list :conn conn :mode mode :root root :state :starting
                  :queue nil :opened nil :versions nil))
      (setf (gethash conn *lsp-conn-keys*) key)
      (rpc-request conn "initialize"
                   (jobj "processId" nil
                         "clientInfo" (jobj "name" "zemacs")
                         "rootUri" (lsp-path-uri root)
                         "rootPath" root
                         "workspaceFolders" nil
                         "capabilities" (%lsp-capabilities))
                   (lambda (result error)
                     (%lsp-initialized key result error)))
      key)))

(defun %lsp-initialized (key result error)
  "The reply to `initialize'. Nothing may be sent to a server before
`initialized' goes out, which is why everything until now was queued.

RESULT is read for exactly two things — whether the server completes at all, and
what shape of `didChange' it will accept. Everything else it advertises we either
already assumed or do not implement, and a client that inspected capabilities it
never acts on would be writing down its own wishes."
  (let ((conn (%lsp-get key :conn)))
    (cond
      (error
       (message (format nil "lsp: initialize failed: ~a" (jget error "message")))
       ;; The child is still running — a server that refuses the handshake does
       ;; not exit on its own, and leaving it would leak a process per attempt.
       (rpc-stop conn)
       (%lsp-forget key))
      (t
       (rpc-notify conn "initialized" :empty-object)
       ;; `completionProvider' is an *object* when the server completes and
       ;; absent when it does not, so its presence is the whole test. Recorded
       ;; rather than asked per keystroke, and honoured rather than ignored:
       ;; a server with no provider answers `textDocument/completion' with
       ;; MethodNotFound, and a popup that produced an error message on every
       ;; word typed would be worse than one that never appears.
       (%lsp-set key :completion (and (jget result "capabilities" "completionProvider") t))
       ;; `textDocumentSync' is either the number itself or an object with a
       ;; `change' in it, and both spellings are shipped — `jget' on an integer
       ;; answers NIL, so asking for the field is safe either way. 2 is
       ;; Incremental and is the *only* value that may be sent a range; 1, 0 and
       ;; a server that says nothing all get the whole document, which is what
       ;; every server got until now. Erring towards full is the whole reason
       ;; this is a test for one value rather than for the absence of another: a
       ;; full-sync server handed a range is a corruption that shows up much
       ;; later, where a range-capable server handed full text is merely slow.
       (%lsp-set key :sync
                 (let ((sync (jget result "capabilities" "textDocumentSync")))
                   (if (eql 2 (if (integerp sync) sync (jget sync "change")))
                       :incremental
                       :full)))
       (%lsp-set key :state :ready)
       ;; In order: the didOpen that started all this has to precede the
       ;; didChanges that piled up behind it.
       (dolist (thunk (reverse (%lsp-get key :queue))) (ignore-errors (funcall thunk)))
       (%lsp-set key :queue nil)
       (message (format nil "lsp: ~a ready in ~a"
                        (%lsp-get key :mode) (%lsp-get key :root)))))))

(defun %lsp-send (key method params)
  "Send a notification, or park it until the handshake finishes.

PARAMS is built *now* even when the send is deferred: it carries the buffer text
as it is at this moment, and a thunk that read it later would send whatever the
buffer had become."
  (let ((conn (%lsp-get key :conn)))
    (if (eq (%lsp-get key :state) :ready)
        (rpc-notify conn method params)
        (%lsp-set key :queue
                  (cons (lambda () (rpc-notify conn method params))
                        (%lsp-get key :queue))))))

(defun %lsp-forget (key)
  "Drop the session without talking to the child — for a server that has already
died or never came up."
  (let ((conn (%lsp-get key :conn)))
    (when conn (remhash conn *lsp-conn-keys*))
    (dolist (path (%lsp-get key :opened)) (remhash path *lsp-shadow*))
    (remhash key *lsp-sessions*)))

;;; ---------------------------------------------------------------------------
;;; Document synchronisation

(defun %lsp-version (key path)
  "The next version number for PATH, remembered per session as LSP requires
them to be monotonic per document."
  (let* ((table (%lsp-get key :versions))
         (v (1+ (or (cdr (assoc path table :test #'string=)) 0))))
    (%lsp-set key :versions
              (cons (cons path v) (remove path table :key #'car :test #'string=)))
    v))

(defun %lsp-content-change (old new)
  "One `contentChanges' entry turning OLD into NEW, or NIL when they are equal.
Both are whole documents; the range is in OLD's coordinates, because those are
the ones the server has.

Common prefix and common suffix, which is what an edit in a text editor looks
like from the outside — one contiguous run replaced. Not the *minimal* edit:
retyping a word that happens to share its middle sends the whole word. It does
not have to be minimal, only true, and `mismatch' from each end is two compiled
scans where a real diff would be interpreted Lisp on every keystroke.

Both ends used to be snapped to a character boundary by `%lsp-char-start',
because both documents were UTF-8 bytes and a prefix comparison stopped *inside*
the `é' whose accent you had just changed — a range naming half a codepoint is
one a server is entitled to do anything at all with. Both are characters now, so
a `mismatch' can only stop between two of them, and the helper is gone."
  (let ((head (mismatch old new)))
    (when head
      (let* ((lo (length old))
             (ln (length new))
             ;; From the other end, capped so the suffix can never reach back
             ;; over the prefix — `aa' becoming `aaa' matches at both ends.
             (tail (min (- lo (or (mismatch old new :from-end t) 0))
                        (- (min lo ln) head)))
             (old-end (- lo tail)))
        (jobj "range" (jobj "start" (%lsp-position-in old head)
                            "end" (%lsp-position-in old old-end))
              "text" (subseq new head (- ln (- lo old-end))))))))

;;; Nothing in this file encodes anything, and saying so is the point of this
;;; note, because for a while the opposite was the rule. Buffer text was one Lisp
;;; character per UTF-8 byte and `dup_utf8' encoded a character string on the way
;;; out, so a document handed straight to `%json-quote' was encoded twice and
;;; then a third time by `%rpc-send'. Every server was told a non-ASCII file
;;; contained mojibake: rust-analyzer, handed `😀', reported the literal's
;;; value as `ÃÂ°ÃÂ…' and put every column after it on that line
;;; **eighteen** places out. The repair was a `utf8-text' on every string leaving
;;; this file, plus a second one inside `json-string', two halves of which
;;; neither was sufficient, and a paragraph here saying nothing may ever drop
;;; either.
;;;
;;; Both are gone. `f_query' in `crates/lisp/src/shim.c' decodes what the editor
;;; answers and `dup_utf8' encodes what goes back, once each, so the shadow, the
;;; buffer and the strings a server sends are all the same kind of string. The
;;; shadow is stored exactly as it was sent, every `subseq' below names
;;; characters, and the one unit still converted is UTF-16 — which LSP asks for
;;; and nothing else in this editor has ever counted in.
;;;
;;; `crates/lisp/tests/lsp_sync.rs' is where the wire bytes are pinned against a
;;; real emoji, and it is the test that fails if any of this is undone.

(defun %lsp-did-open (key path)
  (let ((text (buffer-string)))
    (setf (gethash path *lsp-shadow*) text)
    (%lsp-send key "textDocument/didOpen"
               (jobj "textDocument"
                     (jobj "uri" (lsp-path-uri path)
                           "languageId" (lsp-language-id (%lsp-get key :mode))
                           "version" (%lsp-version key path)
                           "text" text)))
    (%lsp-set key :opened (cons path (%lsp-get key :opened)))))

(defun %lsp-did-change (key path)
  "Send what changed in PATH — a range when the server accepts one, and the whole
document when it does not or when there is no shadow to measure against yet.

Nothing goes out when the text is unchanged. `after-change-hook' fires when the
*revision* moves, and a buffer switch, a mode change or a minor-mode toggle moves
it without touching a character — those used to cost a whole document each."
  (let ((new (buffer-string))
        (old (gethash path *lsp-shadow*)))
    (unless (equal old new)
      (setf (gethash path *lsp-shadow*) new)
      (%lsp-send key "textDocument/didChange"
                 (jobj "textDocument" (jobj "uri" (lsp-path-uri path)
                                            "version" (%lsp-version key path))
                       "contentChanges"
                       (jarr (or (and old
                                      (eq (%lsp-get key :sync) :incremental)
                                      (%lsp-content-change old new))
                                 (jobj "text" new))))))))

(defun lsp-ensure ()
  "Make sure the live buffer's server is running and has the buffer's text.

Called from `after-change-hook', so this runs on every keystroke in a buffer
with a server, and on nothing else: a buffer with no file, a generated buffer,
and a mode with no server registered all fall out on the first test."
  (let ((path (buffer-file-name))
        (mode (major-mode)))
    (when (and path (not (buffer-read-only-p)) (gethash mode *lsp-servers*))
      (let* ((root (%lsp-root-for path))
             (key (%lsp-key mode root)))
        (unless (gethash key *lsp-sessions*) (setf key (%lsp-start mode root)))
        (when key
          (if (member path (%lsp-get key :opened) :test #'string=)
              (%lsp-did-change key path)
              (%lsp-did-open key path))))))
  ;; NIL on purpose, here and in every command below. `eval-string' echoes the
  ;; value of the last form into the status line, so a command that fell out of
  ;; a `rpc-notify' would flash a bare `T' — or, worse, a request id — every
  ;; time you pressed the key.
  nil)

(add-hook '*after-change-functions* 'lsp-ensure)

(defun lsp-did-save ()
  "Tell the server the live buffer was saved."
  (let ((key (lsp-session-for-buffer))
        (path (buffer-file-name)))
    (when (and key path)
      (%lsp-send key "textDocument/didSave"
                 (jobj "textDocument" (jobj "uri" (lsp-path-uri path))))))
  nil)

;;; Advice, which in Common Lisp is `fdefinition' and needs no framework — the
;;; same mechanism `modes.lisp' uses to make a setting revert.
;;;
;;; ponytail: no text in the didSave, and the save itself lands a frame later
;;; (see `docs/threading.org'), so a server that re-reads from disk on save sees
;;; the file one turn stale. Harmless here because `didChange' has already given
;;; it the current buffer — an argument that survived sync going incremental,
;;; since what changed is the *shape* of that notification and not how current
;;; it leaves the server.
(defvar *lsp-save-advised* nil)
(unless *lsp-save-advised*
  (setf *lsp-save-advised* t)
  (let ((inner (fdefinition 'save-file)))
    (setf (fdefinition 'save-file)
          (lambda (&optional path)
            (funcall inner path)
            (ignore-errors (lsp-did-save))))))

;;; ---------------------------------------------------------------------------
;;; Diagnostics
;;;
;;; Stored as data first and shown second, because the *showing* is about to
;;; change: overlays are being built, and flymake-style gutter and inline
;;; rendering attaches to `*lsp-diagnostics-functions*' below without this file
;;; changing at all.

(defvar *lsp-diagnostics* (make-hash-table :test #'equal)
  "Absolute path -> a list of (LINE COLUMN SEVERITY MESSAGE SOURCE).
LINE is 1-based, agreeing with `line-number'; COLUMN is 0-based, agreeing with
`column' — the server counts it in UTF-16 code units and it is converted on the
way in, so this promise is now kept on a line with an emoji in it as well.
SEVERITY is 1 error, 2 warning, 3 information, 4 hint.")

;;; ---------------------------------------------------------------------------
;;; THE OVERLAY SEAM.
;;;
;;; Each function here is called with one argument, the path whose diagnostics
;;; changed, after `*lsp-diagnostics*' has been updated. An overlay-based
;;; renderer hangs off this and needs nothing from this file:
;;;
;;;   (push (lambda (path)
;;;           (when (string= path (buffer-file-name))
;;;             (clear-my-overlays)
;;;             (dolist (d (lsp-diagnostics path))
;;;               (make-overlay-on-line (first d) (face-for (third d))))))
;;;         *lsp-diagnostics-functions*)
;;;
;;; It exists now — see "Diagnostics in the gutter" below, which is written
;;; against this variable and nothing else. The rest of the presentation is
;;; still here beside it: a summary in the echo area when the diagnostics for
;;; the buffer you are looking at change, a command that reads out the one on
;;; the current line, and a listing buffer.
(defvar *lsp-diagnostics-functions* nil
  "Called with a path when its diagnostics change. Where flymake attaches.")

(defun lsp-diagnostics (&optional (path (buffer-file-name)))
  "Every diagnostic for PATH, as a list of (LINE COLUMN SEVERITY MESSAGE SOURCE)."
  (and path (gethash path *lsp-diagnostics*)))

(defun lsp-severity-name (n)
  (case n (1 "error") (2 "warning") (3 "info") (4 "hint") (t "note")))

;;; Diagnostics arrive for whatever the server has looked at, which for a
;;; workspace-wide analyser is mostly *not* the buffer on screen — so their
;;; columns cannot be converted against `line-string'. The shadow copy is the
;;; right ruler and the only one available: it is the very document the server
;;; measured. Split once per publish rather than once per diagnostic, because a
;;; file with a hundred problems in it would otherwise walk itself a hundred
;;; times.
(defun %lsp-shadow-lines (path)
  "PATH's document as a vector of lines, or NIL when there is no shadow of it."
  (let ((text (and path (gethash path *lsp-shadow*))))
    (when text (coerce (split-string text #\Newline) 'vector))))

(defun %lsp-diagnostic-column (lines line character)
  "CHARACTER, which the server counts in UTF-16 code units, as a character column
on 0-based LINE of LINES.

Left alone when there is nothing to measure against — a diagnostic for a file
this client never opened, which is a thing servers do. An ASCII line is the same
number either way, so the untouched answer is the old behaviour rather than a
new wrong one; zeroing it would be worse than both."
  (if lines
      (%lsp-char-column (if (< line (length lines)) (aref lines line) "") character)
      character))

(defun %lsp-publish-diagnostics (params)
  (let* ((path (lsp-uri-path (jget params "uri")))
         (lines (%lsp-shadow-lines path))
         (rows (loop for d in (jget params "diagnostics")
                     for line = (or (jget d "range" "start" "line") 0)
                     collect (list (1+ line)
                                   (%lsp-diagnostic-column
                                    lines line
                                    (or (jget d "range" "start" "character") 0))
                                   (or (jget d "severity") 1)
                                   (or (jget d "message") "")
                                   (or (jget d "source") "")))))
    (when path
      (if rows
          (setf (gethash path *lsp-diagnostics*) rows)
          (remhash path *lsp-diagnostics*))
      (when (equal path (buffer-file-name)) (%lsp-echo-summary rows))
      (dolist (f *lsp-diagnostics-functions*) (ignore-errors (funcall f path))))))

(defun %lsp-echo-summary (rows)
  (let ((errors (count 1 rows :key #'third))
        (warnings (count 2 rows :key #'third)))
    (message (cond ((null rows) "lsp: no diagnostics")
                   (t (format nil "lsp: ~d error~:p, ~d warning~:p" errors warnings))))))

(defun %lsp-diagnostic-text (d)
  "One diagnostic as a line of prose: severity, message, and the server that said
so. The one spelling of a diagnostic there is — the echo area reads it out of
`lsp-diagnostics-at-point' and the pointer reads it off the overlay's
`help-echo', and a keyboard and a mouse asking the same question have to get the
same answer back."
  (format nil "~a: ~a~@[ [~a]~]"
          (lsp-severity-name (third d))
          (fourth d)
          (and (plusp (length (fifth d))) (fifth d))))

(defun lsp-diagnostics-at-point ()
  "Echo the diagnostics on the cursor's line."
  (let* ((line (line-number))
         (here (remove-if-not (lambda (d) (eql (first d) line)) (lsp-diagnostics))))
    (if here
        (message (format nil "~{~a~^ | ~}" (mapcar #'%lsp-diagnostic-text here)))
        (message "no diagnostic on this line"))))

;;; ---------------------------------------------------------------------------
;;; Diagnostics in the gutter
;;;
;;; The first thing hung off the seam above, and it hangs off it exactly the way
;;; the sketch there says: the seam fires with a path, this repaints if that
;;; path is the one on screen, and not a line above this point knows it exists.
;;;
;;; A *mark in the margin* rather than a colour on the offending text, and the
;;; reason is the face list. An overlay's colours are named from `face-list' so
;;; that a theme change recolours them for free — which is the right trade
;;; everywhere else and is the wrong one here, because there is no `error' face
;;; in that list and there is no room to add one from Lisp. Colouring
;;; diagnostics would mean spending `keyword' or `string' on them with
;;; `set-syntax-color', and that changes what every keyword in every buffer
;;; looks like. Severity goes in the *shape* of the mark instead, which no theme
;;; can take away and which reads the same in a light theme and a dark one.
;;;
;;; `gutter' is the property, and it used to be `line-prefix' — which was a bug
;;; and a visible one. A prefix *pushes its line right by its own width*, which
;;; is what a quote bar over a whole passage wants and is exactly wrong for a
;;; mark on one line in fifty: that line then sat a column out from the code
;;; around it, so the indentation you were reading was a lie and every diagnostic
;;; looked like a formatting error of its own. `gutter' draws in the spare column
;;; the line numbers already reserve and moves nothing.
;;;
;;; It is still an overlay, which is the other half of the point: the mark slides
;;; down when you type a line above it and dies with the text it marked — without
;;; waiting for the server to answer again, which on a slow server is most of a
;;; second of the marks pointing at the wrong lines.
;;;
;;; The *message* is on the same overlay, as `help-echo', and the pointer resting
;;; on the mark is what asks for it. That is deliberately not the `after-string'
;;; this note used to predict, and the correction is worth keeping: an
;;; `after-string' pins text to the end of a line and stays until something takes
;;; it off, which is right for an *inline* message — the feature that note was
;;; actually describing, still unbuilt — and wrong for a hover, which belongs at
;;; the pointer and is gone the moment you look away. See `Tooltip' in
;;; `crates/core/src/lib.rs' for the whole of that argument.
;;;
;;; Nothing about the hover is in this file beyond the string: the pointer is in
;;; Rust, the hit test is against the same gutter the mark was drawn in, and no
;;; motion event ever reaches the image. Which is also why the message travels on
;;; the overlay rather than being looked up in `*lsp-diagnostics*' when asked —
;;; the overlay slides with the text and dies with it, so it cannot come to
;;; describe the wrong line, and asking Lisp per pixel is not a thing that could
;;; be afforded.
;;;
;;; ponytail: the *inline* message is still `SPC l e' and still wants virtual
;;; text — a string drawn after a line's end without covering anything — which
;;; `display' is emphatically not: it replaces the cells it covers, so an inline
;;; message would eat the code it is about. The upgrade path is still an
;;; `after-string' overlay property, which is a payload in `overlay.rs' and a
;;; branch in the renderer's line loop.

(defparameter *lsp-diagnostic-marks*
  '((1 . "●") (2 . "▲") (3 . "›") (4 . "·"))
  "Severity -> the mark drawn in the gutter of its line.
One cell each, and no trailing space: this goes in the gutter's spare column
rather than in front of the text, so there is nothing to pad away from.
Severities are 1 error, 2 warning, 3 information, 4 hint, as `*lsp-diagnostics*'
records them. A `defparameter' because it is the whole of the taste here: change
the strings, or set it to NIL to stop drawing marks at all.")

(defvar *lsp-diagnostic-overlays* (make-hash-table :test #'equal)
  "Path -> the overlay handles drawn in that path's buffer.
Per path rather than one list, because an overlay belongs to exactly one buffer
and `delete-overlay' on a handle from another one is silently nothing — a single
list would strand a mark in every file you looked at and never take it off.")

(defvar *lsp-drawn-in* nil
  "The path `%lsp-draw-diagnostics' last painted. See `%lsp-redraw-on-switch'.")

(defun %lsp-diagnostic-overlay (d)
  "Mark D's line with its severity glyph, carrying its message. NIL if there is
nothing there to mark."
  (let* ((line (first d))
         (severity (third d))
         (beg (line-start line))
         ;; The line's own text, so the overlay is *about* what the server
         ;; complained on and dies with it. An empty line has none and
         ;; `make-overlay' answers NIL for an empty range, so fall back to the
         ;; newline itself — the one character an empty line does have, and the
         ;; case that matters, since an unclosed bracket is reported exactly
         ;; there. Only the last line of a buffer that ends without a newline
         ;; has neither, and goes unmarked.
         (end (max (line-end line) (min (1+ beg) (point-max))))
         (ov (and (> end beg) (make-overlay beg end))))
    (when ov
      (overlay-put ov 'gutter
                   (or (cdr (assoc severity *lsp-diagnostic-marks*)) "?"))
      ;; What the pointer resting on that mark says. The same string `SPC l e'
      ;; echoes, from the same function, because the shape of the mark tells you
      ;; only *that* something is wrong and both of these tell you what.
      (overlay-put ov 'help-echo (%lsp-diagnostic-text d))
      ;; Not read by the renderer — it is how these are told from anyone else's
      ;; overlays, the way `folds-in' tells folds from org's bullets.
      (overlay-put ov 'lsp-diagnostic severity))
    ov))

(defun %lsp-draw-diagnostics (&optional (path (buffer-file-name)))
  "Repaint PATH's marks. Silent unless PATH is the buffer on screen.

Silent and not queued: an overlay can only be made in the live buffer, so a file
whose diagnostics arrived while you were looking elsewhere is painted when you
look at it — which is `%lsp-redraw-on-switch', below."
  (when (and path (equal path (buffer-file-name)))
    (dolist (ov (gethash path *lsp-diagnostic-overlays*))
      (ignore-errors (delete-overlay ov)))
    (setf (gethash path *lsp-diagnostic-overlays*)
          (when *lsp-diagnostic-marks*
            (loop for d in (lsp-diagnostics path)
                  for ov = (%lsp-diagnostic-overlay d)
                  when ov collect ov)))
    (setf *lsp-drawn-in* path)
    nil))

(push '%lsp-draw-diagnostics *lsp-diagnostics-functions*)

;;; Diagnostics arrive once; the buffer they are about goes in and out of view
;;; many times. The seam alone would therefore leave a file you came *back* to
;;; wearing whatever marks it had when you left, or none at all.
;;;
;;; `after-change-hook' fires on a buffer switch as well as on an edit — the
;;; revision moves either way — so the switch is noticed here rather than
;;; needing a hook the editor does not have. The cost on the common path, which
;;; is a keystroke, is one string compare.
(defun %lsp-redraw-on-switch ()
  (let ((path (buffer-file-name)))
    (unless (equal path *lsp-drawn-in*)
      (setf *lsp-drawn-in* path)
      (when (and path (lsp-diagnostics path))
        (%lsp-draw-diagnostics path)))))

(add-hook '*after-change-functions* '%lsp-redraw-on-switch)

(defparameter *lsp-diagnostics-file*
  (zemacs-file "diagnostics.txt")
  "Where `lsp-list-diagnostics' writes its listing. A real file for the same
reason the scratch buffer is one: `find-file' is the only way to put the editor
in a different buffer.")

(defun lsp-list-diagnostics ()
  "Every diagnostic from every server, in a buffer, as `path:line:message' —
the same shape ripgrep prints, so `SPC' on a line opens it there.

ponytail: a plain file, and the editor switches to an already-open buffer rather
than re-reading it, so running this twice in a session shows the first listing.
The fix is a `revert-buffer' or a real generated buffer kind, neither of which
exists yet."
  (handler-case
      (let ((rows nil))
        (maphash (lambda (path ds)
                   (dolist (d ds)
                     (push (format nil "~a:~a:~a: ~a" path (first d)
                                   (lsp-severity-name (third d)) (fourth d))
                           rows)))
                 *lsp-diagnostics*)
        (setf rows (sort rows #'string<))
        (ensure-directories-exist *lsp-diagnostics-file*)
        (with-open-file (out *lsp-diagnostics-file*
                             :direction :output :if-exists :supersede
                             :if-does-not-exist :create
                             ;; Characters, like everything else in the image, so
                             ;; the encoder that spells them is the one the file
                             ;; is read back with. This was `:latin-1' for as
                             ;; long as these strings were already-encoded bytes
                             ;; and utf-8 would have encoded them a second time.
                             :external-format :utf-8)
          (if rows
              (dolist (r rows) (write-line r out))
              (write-line "no diagnostics" out)))
        (find-file (namestring *lsp-diagnostics-file*)))
    (error (e) (message (format nil "lsp-list-diagnostics: ~a" e)))))

;;; ---------------------------------------------------------------------------
;;; Incoming

(defun %lsp-notification (conn method params)
  (declare (ignore conn))
  (cond ((string= method "textDocument/publishDiagnostics")
         (%lsp-publish-diagnostics params))
        ;; The two every server sends and nobody has to act on. Silence rather
        ;; than a message: `pylsp' logs its whole startup this way.
        ((or (string= method "window/logMessage")
             (string= method "$/progress")
             (string= method "telemetry/event")))
        ((string= method "window/showMessage")
         (message (format nil "lsp: ~a" (jget params "message"))))))

(defun %lsp-request (conn id method params)
  "Answer a request the *server* made. We advertise almost nothing, so the only
ones that arrive are the ones a server sends regardless — and every one of them
has to be answered or the server waits forever."
  (cond ((string= method "window/workDoneProgress/create") (rpc-respond conn id nil))
        ((string= method "client/registerCapability") (rpc-respond conn id nil))
        ((string= method "workspace/configuration")
         ;; One null per requested item: "no setting, use your default".
         (rpc-respond conn id (mapcar (constantly nil) (jget params "items"))))
        (t (rpc-respond conn id nil
                        (jobj "code" -32601
                              "message" (format nil "~a is not implemented" method))))))

(defun %lsp-exit (conn report)
  (let ((key (gethash conn *lsp-conn-keys*)))
    (remhash conn *lsp-conn-keys*)
    (when key
      (remhash key *lsp-sessions*)
      (message (format nil "lsp: ~a ~a" key report)))))

;;; ---------------------------------------------------------------------------
;;; Commands

(defun lsp ()
  "Start a language server for the live buffer, or report why not."
  (let ((path (buffer-file-name))
        (mode (major-mode)))
    (cond ((null path) (message "lsp: this buffer has no file"))
          ((null (gethash mode *lsp-servers*))
           (message (format nil "lsp: no server registered for ~a" mode)))
          ((lsp-session-for-buffer) (message "lsp: already running"))
          (t (lsp-ensure))))
  nil)

(defun lsp-stop ()
  "Shut the live buffer's server down, politely and then not."
  (let ((key (lsp-session-for-buffer)))
    (if (null key)
        (message "lsp: nothing running here")
        (let ((conn (%lsp-get key :conn)))
          ;; The protocol's own sequence. `rpc-stop' writes what is queued
          ;; before it closes the pipe, so both of these reach the server.
          (dolist (path (%lsp-get key :opened))
            (rpc-notify conn "textDocument/didClose"
                        (jobj "textDocument" (jobj "uri" (lsp-path-uri path)))))
          (rpc-request conn "shutdown" nil (lambda (r e) (declare (ignore r e))))
          (rpc-notify conn "exit")
          (rpc-stop conn)
          (%lsp-forget key)
          (message (format nil "lsp: stopped ~a" key)))))
  nil)

(defun lsp-restart ()
  "Stop and start again — what you reach for after changing a server's config."
  (lsp-stop)
  (lsp-ensure)
  nil)

(defun lsp-status ()
  "What is running, and where."
  (let (rows)
    (maphash (lambda (key plist)
               (push (format nil "~a [~a]" key (getf plist :state)) rows))
             *lsp-sessions*)
    (message (if rows (format nil "lsp: ~{~a~^, ~}" rows) "lsp: nothing running"))))

(defun %lsp-goto-location (loc)
  "Jump to a Location or a LocationLink. `find-file-at' is the only way to open
a file *and* land on a line — a `goto-char' after `find-file' would move the
cursor in the buffer being left."
  (let* ((uri (or (jget loc "uri") (jget loc "targetUri")))
         (range (or (jget loc "range") (jget loc "targetSelectionRange")
                    (jget loc "targetRange")))
         (path (lsp-uri-path uri))
         (line (1+ (or (jget range "start" "line") 0))))
    (if path
        (find-file-at (format nil "~a:~a:" path line))
        (message (format nil "lsp: cannot open ~a" uri)))))

(defun lsp-goto-definition ()
  "Jump to the definition of whatever is under the cursor."
  (let ((key (lsp-session-for-buffer))
        (path (buffer-file-name)))
    (if (null key)
        (message "lsp: no server for this buffer")
        (rpc-request
         (%lsp-get key :conn) "textDocument/definition"
         (jobj "textDocument" (jobj "uri" (lsp-path-uri path))
               "position" (%lsp-point-position))
         (lambda (result error)
           (cond (error (message (format nil "lsp: ~a" (jget error "message"))))
                 ((null result) (message "lsp: no definition found"))
                 ;; A Location has a `uri'; a list of them, or of LocationLinks,
                 ;; does not — take the first, which is what every editor does.
                 ((jget result "uri") (%lsp-goto-location result))
                 ((jget result "targetUri") (%lsp-goto-location result))
                 ((consp result) (%lsp-goto-location (first result)))
                 (t (message "lsp: no definition found")))))))
  nil)

;;; ---------------------------------------------------------------------------
;;; Completion at point — corfu
;;;
;;; A server has been running this whole file and there has been no way to see
;;; anything it knows while you type. This is that: `textDocument/completion',
;;; and a box of candidates hanging off the caret.
;;;
;;; **The popup is `completion-show'/`completion-row', which is which-key's
;;; mechanism one level down** — the editor draws rows the image fills in, and
;;; the editor decides when they stop being true. Not overlays, which is the
;;; answer that keeps looking right and keeps being wrong: an overlay's payload
;;; is drawn *in the flow of the line*, so ten candidates would be ten lines of
;;; document that are not in the file. Not a prompt either, and here that is the
;;; whole feature — a prompt owns the keyboard, and completion works by your
;;; going on typing into the buffer while the list narrows.
;;;
;;; ---------------------------------------------------------------------------
;;; Three clocks, none of them the keyboard's
;;;
;;; Everything hard here is timing, so it is worth being explicit about what is
;;; late and by how much.
;;;
;;; 1. *The hook is a queue turn behind the keystroke* (`docs/threading.org').
;;;    So `lsp-complete-maybe' never trusts what it was told about the edit: it
;;;    re-reads `(point)' and the text behind it and asks "what word is point in
;;;    **now**". That is also why the delta hook is not used. `after-edit-hook'
;;;    would hand us (START OLD-END NEW-END TEXT), and that describes an edit
;;;    which may already be two edits old by the time we run — whereas a read of
;;;    the buffer at hook time is current by construction. A delta is the right
;;;    signal for something reconstructing a *history* — incremental didChange is
;;;    exactly that, and cannot use it either, for the reason at the top of this
;;;    file; it is the wrong one for a question about the present.
;;;
;;; 2. *The server is a subprocess.* The reply is validated against the buffer
;;;    as it is **when it arrives** rather than as it was when we asked — same
;;;    file, anchor unmoved, still a word there — and the candidates are then
;;;    filtered against the prefix re-read at that moment. An answer for a word
;;;    you have stopped typing is dropped rather than shown.
;;;
;;; 3. *The editor retires the popup without telling us.* Point walks out of the
;;;    word, `Esc' leaves Insert, and core hides the box on the spot — the same
;;;    way it empties which-key, and for the same reason: the image cannot see a
;;;    keystroke resolve. So `lsp-complete-accept' asks `(completion-at)' for the
;;;    anchor instead of trusting the one it stored, and inserts nothing when the
;;;    answer is NIL. Otherwise `C-y' would complete a word whose popup is not on
;;;    screen.
;;;
;;; The insertion itself is one `replace-region' over [anchor, point), which is
;;; `docs/threading.org's rule about all-or-nothing sequences: a `delete-region'
;;; and an `insert' have a keystroke's width between them.
;;;
;;; ---------------------------------------------------------------------------
;;; Why a *prefix* match, and not the minibuffer's
;;;
;;; `crates/core/src/minibuffer.rs' has a good matcher — subsequence scoring,
;;; and now orderless on top of it — and this deliberately does not use it.
;;;
;;; In a minibuffer the query is a scratch string that is thrown away when you
;;; accept; here **the query is the document**. A candidate replaces the
;;; characters you typed, so a match that is not a prefix rewrites text you
;;; chose: with subsequence matching `str' offers `from_str', and accepting it
;;; deletes the `str' you meant and leaves a symbol you never asked for. The
;;; looseness that makes `buffer switch' find `switch-to-buffer' is exactly what
;;; you do not want three characters into a word.
;;;
;;; The second reason is that the server *already filtered*, against the position
;;; we gave it and with a real index behind it. A looser client-side matcher
;;; cannot narrow that list, only widen it back out.
;;;
;;; Case-insensitive, though, which is the one looseness that is a prefix: `str'
;;; finding `String' is a completion and not a substitution.

(defparameter *lsp-completion-minimum* 2
  "How many characters of a word must be typed before the popup appears.
One is too few — every identifier in the file matches a single letter, and the
box would be up permanently. `lsp-complete' ignores this: asking explicitly is a
statement that you want the list however short the word is.")

(defparameter *lsp-completion-limit* 20
  "How many candidates to show. Past this the popup is a directory rather than a
suggestion, and you are better off typing another character.")

(defvar *lsp-completion* nil
  "The live popup, or NIL. A plist:

  :AT      buffer offset the word starts at — the popup's anchor, and the start
           of the range a candidate replaces
  :PATH    the file it belongs to, so a reply for another one is dropped
  :ITEMS   every candidate the server sent, as (INSERT FILTER ROW)
  :SHOWN   the ones matching the prefix now, same shape, capped
  :INDEX   which of :SHOWN is highlighted")

;;; Word characters, and **ASCII on purpose** — but for a smaller reason than it
;;; used to be.
;;;
;;; It was a correctness argument once. Buffer text reached Lisp as UTF-8 bytes
;;; while `(point)' counted characters, so scanning back over the text to find
;;; where a word starts answered in the wrong unit — except that every byte of a
;;; multi-byte sequence is >= #x80, so a scan stopping at the first non-ASCII
;;; byte had counted bytes that were each exactly one character. The two units
;;; are one now, and the scan below would be correct over any alphabet at all.
;;;
;;; ponytail: it still stops at the first non-ASCII letter, so `naïve' filters on
;;; `ve' and a Greek variable in a Python file filters on nothing. A shorter
;;; prefix, never a wrong offset, so the worst case is a longer list. The upgrade
;;; is a real word-constituent test — `alpha-char-p' plus the connector
;;; punctuation an identifier may carry — and it is now a one-line change here
;;; rather than a change to the boundary.

(defun %lsp-word-char-p (ch)
  "True for the ASCII characters an identifier is made of."
  (let ((c (char-code ch)))
    (or (<= 65 c 90) (<= 97 c 122) (<= 48 c 57) (= c 95))))

(defun %lsp-completion-prefix ()
  "(AT . PREFIX) for the word point sits at the end of, or NIL when it does not.

AT is a character offset, as is everything else here."
  (let* ((beg (line-start))
         (p (point))
         (text (buffer-substring beg p))
         (n (length text))
         (k 0))
    (loop while (and (< k n) (%lsp-word-char-p (char text (- n k 1))))
          do (incf k))
    (when (plusp k)
      (cons (- p k) (subseq text (- n k))))))

(defun %lsp-prefix-p (prefix candidate)
  "True when CANDIDATE starts with PREFIX, ignoring case."
  (let ((n (length prefix)))
    (and (<= n (length candidate))
         (string-equal prefix candidate :end2 n))))

(defun %lsp-completion-items (result)
  "The items in a `CompletionList', or a bare array of `CompletionItem'.

Both shapes are legal and servers ship both. A decoded object is an alist, so
`jget' answers NIL for the array case without being told which one it has —
`%json-object-p' is only needed to tell an empty list apart from an object that
happens to have no items."
  (cond ((null result) nil)
        ((jget result "items"))
        ((%json-object-p result) nil)
        ((listp result) result)))

;;; What a candidate *is*, in the vocabulary the editor already has.
;;;
;;; LSP's `CompletionItemKind' is twenty-five numbers and the editor's face list
;;; is ten names, so this is a lossy map on purpose: the popup draws a badge in
;;; the theme's colour for that face, which means a function in the list is the
;;; same hue as a function call in the buffer behind it. Sending a *colour* or a
;;; *glyph* from here would put the taste in the client and take it away from
;;; the theme, which is the trade `*lsp-diagnostic-marks'" already refused for
;;; the gutter and refuses again here.
(defparameter *lsp-completion-kinds*
  '((2 . "function") (3 . "function") (4 . "function")   ; method, function, ctor
    (5 . "variable") (6 . "variable") (10 . "variable")  ; field, variable, property
    (7 . "type") (8 . "type") (13 . "type") (22 . "type") (25 . "type")
    (9 . "type") (17 . "string") (19 . "string")         ; module, file, folder
    (14 . "keyword") (15 . "keyword")                    ; keyword, snippet
    (12 . "constant") (20 . "constant") (21 . "constant")
    (24 . "operator") (11 . "number") (16 . "string"))
  "`CompletionItemKind' -> the face its badge is drawn in. Anything not here —
`Text', `Reference', `Event' — falls back to `default', which is the honest
answer for a candidate whose kind says nothing about how to read it.")

(defun %lsp-completion-kind (item)
  (or (cdr (assoc (jget item "kind") *lsp-completion-kinds*)) "default"))

(defun %lsp-doc-string (item)
  "The `documentation' on ITEM as a string, or NIL.

Two shapes are legal — a bare string, and a `MarkupContent' with `kind' and
`value' — and servers ship both, which is why this is a function rather than a
`jget'."
  (let ((doc (jget item "documentation")))
    (cond ((stringp doc) doc)
          ((null doc) nil)
          (t (jget doc "value")))))

(defun %lsp-completion-row (item)
  "(INSERT FILTER ROW DOC) for one `CompletionItem', or NIL for one with no label.

INSERT is what goes in the buffer, FILTER is what the prefix is tested against,
ROW is what is drawn and DOC is what the panel beside the list shows — four
fields because LSP says the first three can differ, and a server that sets
`filterText' has told us the label is not what to match on.

ROW is tab-separated into KIND, LABEL and DETAIL, which the renderer draws as
three aligned columns. Tabs and not spaces because the *alignment* has to be
done where cells can be measured, and this side of the boundary cannot measure
one — the old two-space join is exactly what that produced: a ragged second
column.

ponytail: `textEdit' is ignored, so a server whose edit range is not exactly
[anchor, point) — clangd replacing a whole call, a server that completes past a
`.' — inserts the plain label instead. Ceiling: those completions come out
slightly wrong rather than not at all. Upgrade path: read `textEdit.range',
convert its line/character pair through `line-start', and pass that range to
`replace-region' instead of the anchor. `snippetSupport' is declined in
`%lsp-capabilities' precisely so nothing arrives with `$1' in it meanwhile."
  (let* ((label (or (jget item "label") ""))
         (insert (or (jget item "insertText") label))
         (filter (or (jget item "filterText") label))
         (detail (or (jget item "detail") "")))
    (when (plusp (length label))
      (list insert filter
            (format nil "~a~c~a~c~a"
                    (%lsp-completion-kind item) #\Tab label #\Tab detail)
            (%lsp-doc-string item)))))

;;; ---------------------------------------------------------------------------
;;; The documentation panel
;;;
;;; A docstring is prose and the box it goes in is a fixed number of columns, so
;;; something has to wrap it. That something is here rather than in the renderer
;;; for the reason every other layout decision went the other way: wrapping is
;;; about *words*, not cells, and a greedy fill on spaces is right at any font.
;;;
;;; ponytail: wrapped on ASCII spaces and counted in characters, so a CJK
;;; docstring wraps *early* — a character there is two cells wide, and only the
;;; renderer knows that. The upgrade is a width reader beside `highlight', which
;;; is the same answer the `line-overflow' code wants. A markdown
;;; docstring is shown as its source: no renderer, and fences and backticks read
;;; well enough that stripping them would lose more than it hides.

(defparameter *lsp-doc-columns* 56
  "Where a documentation line is wrapped. Matches `DOC_POPUP_COLS' in the
renderer — wrapping wider only means the box truncates what this already fitted.")

(defparameter *lsp-doc-lines* 10
  "Most lines of documentation shown. Matches `POPUP_ROWS': past this the panel
is a manual page hanging off your cursor.")

;;; `split-string' is `modes.lisp''s. It used to be spelled out here *and* in
;;; `ai.lisp', identical down to the docstring, under a comment saying this file
;;; was loadable on its own and that was worth two lines. Two lines was the
;;; wrong price to compare against: what it actually bought was a second copy of
;;; a function, and the standard library is loaded above this one either way.

(defun %lsp-first-n (list n)
  "The first N elements of LIST, or all of them if it is shorter."
  (subseq list 0 (min n (length list))))

(defun %lsp-wrap (text columns)
  "TEXT as a list of lines no wider than COLUMNS, breaking on spaces.

Existing newlines are kept — a docstring's paragraph breaks are the only
structure it has — and a word longer than COLUMNS is left long rather than
split, since the one thing in a docstring that is too wide to break is usually
a type or a path."
  (let ((out nil))
    (dolist (para (split-string text #\Newline) (nreverse out))
      (let ((line ""))
        (dolist (word (remove "" (split-string para #\Space) :test #'string=))
          (cond ((zerop (length line)) (setf line word))
                ((<= (+ (length line) 1 (length word)) columns)
                 (setf line (concatenate 'string line " " word)))
                (t (push line out) (setf line word))))
        (push line out)))))

(defun %lsp-completion-doc-draw ()
  "Send the selected candidate's documentation to the panel.

Cleared and refilled rather than diffed, and resent on every selection move:
the panel is about one candidate, so there is nothing to keep. `completion-doc'
with no argument is the clear, which is also what a candidate with no
documentation sends — an empty panel is not drawn at all."
  (let* ((rows (getf *lsp-completion* :shown))
         (row (nth (getf *lsp-completion* :index) rows))
         (doc (fourth row)))
    (completion-doc)
    (when (and doc (plusp (length doc)))
      (dolist (line (%lsp-first-n (%lsp-wrap doc *lsp-doc-columns*) *lsp-doc-lines*))
        (completion-doc line))))
  nil)

(defun %lsp-completion-hide ()
  "Take the popup down and forget what was in it.

The *forgetting* is the whole difference between this and the bare
`completion-show' in `%lsp-completion-draw', and only three things are entitled
to it: abandoning the word, accepting a candidate, and `C-e'. A no-op when there
is nothing up, so the common keystroke in a buffer with a server — Insert mode,
no word under point — costs a NIL test rather than a command down the channel."
  (when *lsp-completion*
    (setf *lsp-completion* nil)
    (completion-show))
  nil)

(defun %lsp-completion-draw ()
  "Send the current candidates and selection to the editor.

`completion-show' first and `completion-row' after, in that order: rows land in
a popup that exists, and showing at an anchor that has not moved keeps whatever
rows are already there — so the explicit clear is what makes this a replacement
rather than an append."
  (let ((rows (getf *lsp-completion* :shown)))
    (cond
      (rows
       (completion-show (getf *lsp-completion* :at) (getf *lsp-completion* :index))
       (completion-row)
       (dolist (r rows) (completion-row (third r)))
       (%lsp-completion-doc-draw))
      ;; Nothing to draw — and the state stays, in both of the ways this
      ;; happens.
      ;;
      ;; While the request is still out there is nothing to match against yet,
      ;; and dropping the plist here would throw away the very thing the reply
      ;; is checked against: typing a third character would *cancel the request
      ;; the second one triggered*, and the popup would only ever appear if you
      ;; stopped typing at exactly two letters.
      ;;
      ;; And once the items are in, a prefix that matches none of them is one
      ;; backspace away from matching again — so keeping them is what stops
      ;; every further keystroke re-asking the server about the same position.
      (t (completion-show))))
  nil)

(defun %lsp-completion-filter (prefix)
  "Narrow the candidates we already have to PREFIX and redraw."
  (let ((rows (loop for c in (getf *lsp-completion* :items)
                    when (%lsp-prefix-p prefix (second c)) collect c)))
    (setf (getf *lsp-completion* :shown)
          (subseq rows 0 (min (length rows) *lsp-completion-limit*))
          (getf *lsp-completion* :index) 0)
    (%lsp-completion-draw)))

(defun %lsp-completion-reply (path at result error)
  "A server's answer, checked against the buffer as it is *now*.

Four ways an answer is stale and all four are the same answer — drop it: the
request was superseded, you are in another file, the word moved, or you stopped
typing a word at all. Nothing is retried, because the next keystroke asks again."
  (let ((live *lsp-completion*))
    (when (and live
               (null error)
               (eql at (getf live :at))
               (equal path (buffer-file-name)))
      (setf (getf *lsp-completion* :items)
            (loop for item in (%lsp-completion-items result)
                  for row = (%lsp-completion-row item)
                  when row collect row))
      (let ((where (%lsp-completion-prefix)))
        (if (and where (eql (car where) at))
            (%lsp-completion-filter (cdr where))
            (%lsp-completion-hide))))))

(defun %lsp-completion-request (at)
  "Ask the server what can follow the word starting at AT.

The old popup is replaced *before* the request goes out rather than left up
until the reply, so the anchor recorded here is what the reply is checked
against. A popup showing the previous word's candidates while the next word's
are in flight is the one stale state worth spending a frame of emptiness on."
  (let ((key (lsp-session-for-buffer))
        (path (buffer-file-name)))
    (cond
      ;; No server, or one that declined to complete. The old popup still has to
      ;; come down: point has moved to a word this list is not about, and
      ;; leaving it up would be the one thing worse than showing nothing.
      ((not (and key path (%lsp-get key :completion))) (%lsp-completion-hide))
      (t
       (setf *lsp-completion*
             (list :at at :path path :items nil :shown nil :index 0))
       (rpc-request
        (%lsp-get key :conn) "textDocument/completion"
        (jobj "textDocument" (jobj "uri" (lsp-path-uri path))
              "position" (%lsp-point-position)
              ;; 1 is `Invoked' — this client has no trigger characters, since
              ;; deciding to complete is a rule in this file rather than a
              ;; property of the server.
              "context" (jobj "triggerKind" 1))
        (lambda (result error) (%lsp-completion-reply path at result error)))
       nil))))

(defun lsp-complete-maybe (&optional force)
  "Keep the popup in step with the word point is in. On `*after-change-functions*'.

Cheap on the path that matters, which is a keystroke in a buffer with no server:
one string compare on the mode, then one hash lookup, and out.

Re-asks the server only when the word *starts* somewhere new. Typing further
into a word we already have candidates for narrows them locally, which is what
makes the popup feel instant — and is legitimate, because the server's list for
`fo' contains its list for `foo'. ponytail: `isIncomplete' is ignored, so a
server that truncated its answer is not asked again and the list can be short by
whatever it left out. Ceiling: a huge global namespace two characters in.
Upgrade path is one `jget' and a flag on the plist that forces the re-ask."
  (when (string= (evil-state) "insert")
    (let ((where (and (lsp-session-for-buffer) (%lsp-completion-prefix))))
      (cond
        ((null where) (%lsp-completion-hide))
        ((and (not force) (< (length (cdr where)) *lsp-completion-minimum*))
         (%lsp-completion-hide))
        ((and *lsp-completion*
              (not force)
              (eql (car where) (getf *lsp-completion* :at))
              (equal (buffer-file-name) (getf *lsp-completion* :path)))
         (%lsp-completion-filter (cdr where)))
        (t (%lsp-completion-request (car where))))))
  nil)

(add-hook '*after-change-functions* 'lsp-complete-maybe)

;;; ---------------------------------------------------------------------------
;;; The keys
;;;
;;; `TAB' cycles, `S-TAB' cycles back and `RET' accepts — corfu's bindings — and
;;; **none of those three is bound here**. They cannot be: a Lisp command bound
;;; to `RET' would have to insert the newline itself when no popup was up, and it
;;; would insert it a queue turn late, *after* whatever you typed next, so
;;; `a<RET>b' would come out as `ab' and a newline (`docs/threading.org').
;;;
;;; So core asks the question instead, in `Evil::insert_key': is a popup on
;;; screen *right now*? It knows synchronously, because it is what decides — and
;;; a question with no fallback in it is a question the image cannot answer. The
;;; three keys reach this file as `lsp-complete-next' and friends, which is why
;;; those stay commands even though nothing here binds them.
;;;
;;; `C-n' `C-p' `C-y' `C-e' are bound here, and stay: vim's insert-mode spelling,
;;; every one of them a dead key in Insert mode otherwise, and muscle memory for
;;; anyone who came from `C-x C-o'.

(defun %lsp-completion-move (delta)
  "Move the selection by DELTA, wrapping.

One `completion-show' and no rows: the candidates have not changed, and
resending them would be one command per candidate on a keypress whose whole
point is to be cheap.

The *documentation* does go again, because it is about the candidate rather than
about the list — a dozen short lines against a hundred candidates, which is
exactly why those are two verbs with two lifetimes."
  (let ((rows (and *lsp-completion* (getf *lsp-completion* :shown))))
    (when rows
      (let ((i (mod (+ (getf *lsp-completion* :index) delta) (length rows))))
        (setf (getf *lsp-completion* :index) i)
        (completion-show (getf *lsp-completion* :at) i)
        (%lsp-completion-doc-draw))))
  nil)

(defun lsp-complete-next ()
  "Highlight the next candidate. Nothing when no popup is up."
  (%lsp-completion-move 1))

(defun lsp-complete-previous ()
  "Highlight the previous candidate."
  (%lsp-completion-move -1))

(defun lsp-complete-abort ()
  "Dismiss the popup, leaving what you typed alone."
  (%lsp-completion-hide))

(defun lsp-complete-accept ()
  "Replace the word being typed with the highlighted candidate.

The anchor comes from the *editor* — `(completion-at)' — rather than from our own
plist, because core hides a popup point has walked out of without saying so, and
completing a word whose box is not on screen is the one failure this command can
have. NIL means there is nothing to accept."
  (let ((at (completion-at))
        (row (and *lsp-completion*
                  (nth (getf *lsp-completion* :index)
                       (getf *lsp-completion* :shown)))))
    (cond ((or (null at) (null row)) (%lsp-completion-hide))
          (t
           ;; One primitive over the whole range: `docs/threading.org'. A
           ;; `delete-region' and an `insert' would be two undo steps with a
           ;; keystroke's width between them.
           ;;
           ;; ponytail: `at' and `(point)' are two reads, so a keystroke landing
           ;; between them widens the range by a character. Same window every
           ;; two-reader command in this editor has; closing it means the range
           ;; travelling with the command.
           (replace-region at (point) (first row))
           (%lsp-completion-hide))))
  nil)

(defun lsp-complete ()
  "Ask for completions here and now, however little of the word is typed."
  (cond ((null (lsp-session-for-buffer)) (message "lsp: no server for this buffer"))
        ((not (string= (evil-state) "insert")) (message "lsp: completion is an Insert-mode thing"))
        (t (lsp-complete-maybe t)))
  nil)

;;; ponytail: `hover' and `signatureHelp' are the obvious next two and are
;;; deliberately not here. Both are the same round trip with a different method
;;; name, and both want a surface this editor does not have — a *transient
;;; paragraph* near point, which is neither the one-line echo area nor a box you
;;; navigate. Building them on `completion-show' would give you a popup whose
;;; rows are wrapped prose, which is the wrong shape twice.

;;; ---------------------------------------------------------------------------
;;; What ships
;;;
;;; Two servers, because these are the two the client was proved against. A
;;; third is one line, and belongs in your config rather than here.

(lsp-register-server 'python-mode "pylsp")
(lsp-register-server 'c-mode "clangd" "--background-index")

;;; Readers and internals are not commands: running `lsp-diagnostics' by hand
;;; builds a list nobody sees. `*hidden-commands*' is the init file's lever, and
;;; is only bound when this file is loaded from one.
(when (boundp '*hidden-commands*)
  (dolist (n '("lsp-diagnostics" "lsp-ensure" "lsp-did-save" "after-change-hook"
               "lsp-session-for-buffer"
               ;; The completion keys. `M-x lsp-complete-next' with no popup up
               ;; does nothing at all, which is not a command — it is half of a
               ;; key binding, and the other half is the popup.
               "lsp-complete-maybe" "lsp-complete-next" "lsp-complete-previous"
               "lsp-complete-accept" "lsp-complete-abort"
               ;; The popup's own verbs, which take an argument and answer
               ;; nothing: running one by hand draws half a box.
               "completion-doc" "completion-row" "completion-show"))
    (pushnew n (symbol-value '*hidden-commands*) :test #'string=)))

(export '(lsp lsp-stop lsp-restart lsp-status lsp-goto-definition
          lsp-diagnostics lsp-diagnostics-at-point lsp-list-diagnostics
          lsp-register-server lsp-project-root lsp-path-uri lsp-uri-path
          lsp-severity-name *lsp-diagnostics-functions* *lsp-servers*
          *after-change-functions*
          lsp-complete lsp-complete-next lsp-complete-previous
          lsp-complete-accept lsp-complete-abort *lsp-completion-minimum*
          *lsp-completion-limit*)
        :zemacs)
