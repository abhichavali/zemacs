;;;; JSON-RPC over stdio — talking to a long-lived child from Lisp.
;;;;
;;;; The Rust half (`crates/rpc') owns the pipe: spawning, Content-Length
;;;; framing, id allocation, draining the child's stderr, noticing it die. It
;;;; knows nothing about what the messages mean. Everything that *is* protocol —
;;;; which request to send, what to do with the reply, which notification
;;;; matters — is here, because that is the part every config wants to bend.
;;;;
;;;; `lsp.lisp' next door is the first user. It is deliberately not the only
;;;; possible one: nothing below mentions a language server.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; How an answer gets back
;;;;
;;;; It does not come back from the call that sent the request. `rpc-request'
;;;; returns immediately with an id — the child has not been asked yet, let
;;;; alone answered. What happens is:
;;;;
;;;;   1. a reader thread in Rust parses the child's frame and pushes it onto a
;;;;      channel;
;;;;   2. the *main* thread drains that channel once a frame, turns the JSON
;;;;      into a Lisp form, and queues `(%rpc-event CONN KIND FORM)' for the
;;;;      Lisp thread — exactly the route a mode hook takes;
;;;;   3. `%rpc-event' runs here, on the Lisp thread, looks the id up in the
;;;;      connection's pending table, and calls the closure parked under it.
;;;;
;;;; No foreign thread ever calls into ECL, nothing blocks the editor, and the
;;;; continuation is an ordinary Lisp closure rather than something Rust has to
;;;; hold a handle on. The cost is the one `docs/threading.org' names: your
;;;; callback runs *later*, on a turn of the Lisp queue of its own, so it cannot
;;;; return a value to whoever asked. Write it as a continuation.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; JSON
;;;;
;;;; Decoding happens in Rust, and a message arrives here already READ:
;;;;
;;;;   null    -> NIL          [a b]     -> (a b)
;;;;   true    -> T            {"k": v}  -> (("k" . v))   an alist
;;;;   false   -> NIL          number    -> integer or double
;;;;
;;;; so `false', `null', `[]' and `{}' are all NIL and `(jget d "severity")'
;;;; cannot tell absent from false. That is the mapping that makes a Lisp reader
;;;; read naturally, and nothing in LSP depends on the distinction.
;;;;
;;;; Encoding is here, in `json-encode', because building a request *is*
;;;; protocol. Only the escaping is a primitive (`%json-quote'), and only
;;;; because the string it escapes most often is a whole buffer.
;;;;
;;;; What a caller writes:
;;;;
;;;;   (rpc-start "pylsp" :cwd root :on-notify #'handler)      => CONN or NIL
;;;;   (rpc-request conn "initialize" params (lambda (r e) ...)) => ID or NIL
;;;;   (rpc-notify conn "initialized" :empty-object)
;;;;   (rpc-respond conn id result)
;;;;   (rpc-stop conn)
;;;;   (jget message "params" "uri")

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; Encoding
;;;
;;; ponytail: whether a list is a JSON object or a JSON array is *inferred* —
;;; a list whose first element is a cons with an atom in its car is an alist and
;;; becomes an object, and anything else becomes an array. That gets every shape
;;; a protocol actually sends right, including an array of objects, and gets
;;; `((1 2) (3 4))' wrong. Use `jobj' and `jarr' rather than writing the
;;; literals by hand and the question does not come up. The upgrade, if
;;; something ever needs `[[1,2]]', is a wrapper struct — one arm of the COND.

(defun json-string (s)
  "S as a JSON string literal, quotes and escapes included."
  (%json-quote (string s)))

(defun %json-object-p (x)
  "True for a list that should be encoded as a JSON object."
  (and (consp x) (consp (car x)) (atom (caar x)) (caar x)))

(defun json-encode (x)
  "X as JSON text.

  NIL              null            a string     a JSON string
  T / :true        true            an integer   a number
  :false           false           a float      a number
  :empty-object    {}              an alist     an object
  :empty-array     []              any list     an array

An empty object and an empty array need their own keywords because NIL is
already `null' — the one place the decoding above has to be spelled out on the
way back."
  (let ((*read-default-float-format* 'double-float))
    (cond ((null x) "null")
          ((eq x t) "true")
          ((eq x :true) "true")
          ((eq x :false) "false")
          ((eq x :null) "null")
          ((eq x :empty-object) "{}")
          ((eq x :empty-array) "[]")
          ((stringp x) (json-string x))
          ((integerp x) (princ-to-string x))
          ((floatp x) (princ-to-string x))
          ((%json-object-p x)
           (format nil "{~{~a~^,~}}"
                   (mapcar (lambda (pair)
                             (format nil "~a:~a" (json-string (car pair))
                                     (json-encode (cdr pair))))
                           x)))
          ((listp x)
           (format nil "[~{~a~^,~}]" (mapcar #'json-encode x)))
          ;; A symbol, a character, anything else: its printed name, as a
          ;; string. Failure is a value here as everywhere else — a config that
          ;; hands us something odd gets a wrong request, not a broken image.
          (t (json-string (princ-to-string x))))))

(defun jobj (&rest pairs)
  "An object from alternating keys and values: (jobj \"line\" 3 \"character\" 0).
NIL with no pairs, which encodes as `null'; use :empty-object for `{}'."
  (loop for (k v) on pairs by #'cddr collect (cons (string k) v)))

(defun jarr (&rest items)
  "An array. NIL when empty, which encodes as `null'; use :empty-array for `[]'."
  items)

(defun jget (object &rest keys)
  "Walk OBJECT — a decoded JSON object — following KEYS, and answer what is
there or NIL: (jget msg \"params\" \"range\" \"start\" \"line\").

Never signals. A key that is not there, a value that is not an object, and a
message shaped differently from what you expected all answer NIL, which is the
convention every reader in this API follows."
  (dolist (k keys object)
    (setf object (and (consp object)
                      (cdr (assoc k object :test #'equal))))))

;;; ---------------------------------------------------------------------------
;;; Connections
;;;
;;; DEFVAR and not DEFPARAMETER: `M-x reload-config' re-reads this file, and a
;;; table of live connections that resets itself would leak every running child
;;; and lose every pending continuation.

(defvar *rpc-connections* (make-hash-table)
  "CONN -> a plist:
  :name        what to call it in a message
  :pending     request id -> the closure waiting for that reply
  :on-notify   (CONN METHOD PARAMS)
  :on-request  (CONN ID METHOD PARAMS) — must answer with `rpc-respond'
  :on-exit     (CONN REPORT)")

(defun rpc-live-p (conn)
  "True while CONN names a running child."
  (and conn (integerp conn) (nth-value 1 (gethash conn *rpc-connections*))))

(defun rpc-connections ()
  "Every live connection, as a list of (CONN NAME)."
  (let (out)
    (maphash (lambda (c plist) (push (list c (getf plist :name)) out))
             *rpc-connections*)
    (nreverse out)))

(defun rpc-start (program &key args cwd name on-notify on-request on-exit)
  "Start PROGRAM as a JSON-RPC child and answer its connection, or NIL.

ARGS is a list of strings. CWD is where to start it, which for a language server
decides what project it thinks it is looking at. The three handlers are called
on the Lisp thread, later, from `%rpc-event'.

A program that is not installed answers NIL and says so in the status line
rather than signalling: a config naming a server you do not have should still
load."
  (let ((conn (%rpc-start (string program)
                          (if args (json-encode args) "")
                          (and cwd (string cwd)))))
    (when (and (integerp conn) (plusp conn))
      (setf (gethash conn *rpc-connections*)
            (list :name (or name (string program))
                  :pending (make-hash-table)
                  :on-notify on-notify
                  :on-request on-request
                  :on-exit on-exit))
      conn)))

(defun rpc-request (conn method params callback)
  "Send METHOD with PARAMS and park CALLBACK under the id it was given.

CALLBACK is called with (RESULT ERROR), exactly one of which is meaningful, on a
later turn of the Lisp queue. Answers the id, or NIL if the message could not be
sent — a connection that has died since you last looked is the usual reason.

PARAMS is a Lisp value encoded by `json-encode'; NIL sends no params at all,
which is not the same as sending `null' and matters to servers that check."
  (let ((id (and (rpc-live-p conn)
                 (%rpc-send conn (string method)
                            (and params (json-encode params)) t))))
    (when (and (integerp id) (plusp id))
      (setf (gethash id (getf (gethash conn *rpc-connections*) :pending)) callback)
      id)))

(defun rpc-notify (conn method &optional params)
  "Send METHOD with no reply expected. Answers T when it went out."
  (when (rpc-live-p conn)
    (%rpc-send conn (string method) (and params (json-encode params)) nil)
    t))

(defun rpc-respond (conn id result &optional error)
  "Answer a request the *child* made. ERROR, when given, replaces RESULT and
should be an object with `code' and `message'."
  (when (rpc-live-p conn)
    (%rpc-respond conn (json-encode id)
                  (and (null error) (json-encode result))
                  (and error (json-encode error)))
    t))

(defun rpc-stop (conn)
  "Hang up on CONN. Anything already queued is written first, so a polite
`exit' sent immediately before this still reaches the child."
  (when conn
    (remhash conn *rpc-connections*)
    (%rpc-stop conn))
  nil)

;;; ---------------------------------------------------------------------------
;;; The other direction
;;;
;;; `%rpc-event' is called by the application, once per message, on the Lisp
;;; thread. It is the only entry point Rust knows the name of.

(defun %rpc-message (conn plist msg)
  "Route one decoded JSON-RPC message. The three shapes are told apart exactly
as the specification tells them apart: a response has an id and no method, a
request has both, a notification has only a method."
  (let ((id (jget msg "id"))
        (method (jget msg "method")))
    (cond
      ((and id (null method))
       (let* ((pending (getf plist :pending))
              (k (gethash id pending)))
         (remhash id pending)
         ;; A reply to a request nobody is waiting for is dropped in silence:
         ;; it is what a `rpc-stop' racing an in-flight request looks like.
         (when k (funcall k (jget msg "result") (jget msg "error")))))
      ((and id method)
       (let ((handler (getf plist :on-request)))
         (if handler
             (funcall handler conn id method (jget msg "params"))
             ;; JSON-RPC insists a request be answered, even to refuse it — a
             ;; server left waiting on `client/registerCapability' can sit there
             ;; forever.
             (rpc-respond conn id nil
                          (jobj "code" -32601 "message"
                                (format nil "~a is not implemented" method))))))
      (method
       (let ((handler (getf plist :on-notify)))
         (when handler (funcall handler conn method (jget msg "params"))))))))

(defun %rpc-event (conn kind form)
  "One thing a child did. KIND is :MESSAGE, :ERROR or :EXIT.

Everything is inside a HANDLER-CASE: a handler that signals must cost you that
one message, not the connection and not the editor. The same argument as
`eval-string', for the same reason."
  (handler-case
      (let ((plist (gethash conn *rpc-connections*)))
        (when plist
          (case kind
            (:message (%rpc-message conn plist form))
            (:error (message (format nil "~a: ~a" (getf plist :name) form)))
            (:exit
             ;; Removed first: a handler that restarts the connection must not
             ;; find the dead one still in the table.
             (remhash conn *rpc-connections*)
             (let ((handler (getf plist :on-exit)))
               (if handler
                   (funcall handler conn form)
                   (message (format nil "~a ~a" (getf plist :name) form))))))))
    (error (e) (message (format nil "rpc handler error: ~a" e)))))

;;; None of these is a command: `jobj' and `jarr' take &rest and so *look*
;;; zero-argument to the introspection `refresh-commands' does, and running
;;; either one by hand builds a value nobody sees. `*hidden-commands*' is the
;;; init file's lever, and is only bound when this file is loaded from one.
(when (boundp '*hidden-commands*)
  (dolist (n '("jobj" "jarr" "rpc-connections"))
    (pushnew n (symbol-value '*hidden-commands*) :test #'string=)))

(export '(json-encode json-string jobj jarr jget
          rpc-start rpc-request rpc-notify rpc-respond rpc-stop
          rpc-live-p rpc-connections)
        :zemacs)
