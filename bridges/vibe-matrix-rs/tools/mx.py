#!/usr/bin/env python3
"""Pomocniczy klient Matrix dla vibe-matrix-rs (bez sekretów w repo).

Token bota/admina czyta ze zmiennej MATRIX_ACCESS_TOKEN lub z pliku
~/.config/vibe-matrix-rs/admin.env (chmod 600, poza repo).

Użycie:
  mx.py send <room_id> <tekst...>     # wyślij jako admin (MATRIX_ADMIN_TOKEN lub login z admin.env)
  mx.py read <room_id> [limit]        # ostatnie wiadomości
  mx.py rooms                         # lista pokoi admina
"""
import json, os, re, sys, time, urllib.request, urllib.parse

HS = os.environ.get("MATRIX_HOMESERVER", "https://matrix.owl-post.eu")
ENVF = os.path.expanduser("~/.config/vibe-matrix-rs/admin.env")

def load_admin():
    tok = os.environ.get("MATRIX_ADMIN_TOKEN")
    user = os.environ.get("MATRIX_ADMIN_USER", "stanislaw")
    pw = os.environ.get("MATRIX_ADMIN_PASSWORD")
    if not tok and os.path.exists(ENVF):
        vals = dict(re.findall(r"^([A-Z_]+)=(.*)$", open(ENVF).read(), re.M))
        tok = tok or vals.get("MATRIX_ACCESS_TOKEN")
        user = vals.get("MATRIX_USER", user)
        pw = pw or vals.get("MATRIX_PASSWORD")
    if not tok and pw:
        st, body = req("POST", "/_matrix/client/v3/login",
                       body={"type": "m.login.password", "user": user, "password": pw})
        if st == 200:
            tok = body["access_token"]
            cache = os.path.expanduser("~/.config/vibe-matrix-rs/.admin-token")
            os.makedirs(os.path.dirname(cache), exist_ok=True)
            with open(cache, "w") as f:
                json.dump({"token": tok}, f)
            os.chmod(cache, 0o600)
    if not tok and os.path.exists(os.path.expanduser("~/.config/vibe-matrix-rs/.admin-token")):
        tok = json.load(open(os.path.expanduser("~/.config/vibe-matrix-rs/.admin-token")))["token"]
    if not tok:
        raise SystemExit("brak tokena admina: ustaw MATRIX_ADMIN_TOKEN lub admin.env")
    return tok

def req(method, path, token=None, body=None):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(HS + path, data=data, method=method)
    r.add_header("Content-Type", "application/json")
    if token:
        r.add_header("Authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(r, timeout=60) as resp:
            return resp.status, json.load(resp)
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read())
        except Exception:
            return e.code, {}

def enc(s):
    return urllib.parse.quote(s, safe="")

cmd = sys.argv[1] if len(sys.argv) > 1 else "read"
tok = load_admin()

if cmd == "send":
    room, text = sys.argv[2], " ".join(sys.argv[3:])
    st, body = req("PUT", f"/_matrix/client/v3/rooms/{enc(room)}/send/m.room.message/{time.time_ns()}",
                   token=tok, body={"msgtype": "m.text", "body": text})
    print("OK" if st == 200 else f"FAIL {st} {body}", body.get("event_id", ""))
elif cmd == "read":
    room = sys.argv[2]
    limit = int(sys.argv[3]) if len(sys.argv) > 3 else 10
    st, msgs = req("GET", f"/_matrix/client/v3/rooms/{enc(room)}/messages?dir=b&limit={limit}", token=tok)
    if st != 200:
        raise SystemExit(f"FAIL {st} {msgs}")
    for ev in reversed(msgs["chunk"]):
        if ev.get("type") == "m.room.message":
            print(f"[{ev['sender']}] {ev.get('content',{}).get('body','')}")
        elif ev.get("type") == "m.reaction":
            print(f"   ~~ {ev['content'].get('m.relates_to',{}).get('key')} od {ev['sender']}")
elif cmd == "rooms":
    st, body = req("GET", "/_matrix/client/v3/joined_rooms", token=tok)
    for r in body.get("joined_rooms", []):
        print(r)
