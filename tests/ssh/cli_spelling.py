"""The ssh tools' end-to-end tests were written in OpenSSH's spelling (`-p 22 -tt
host ls -l`). The tools now speak Oro's grammar (docs/cli-conventions.md): `host
"ls -l" --port 22 --tty-force`. Rather than rewrite two hundred call sites, this
module translates an OpenSSH-style argument list into the new spelling at the one
place a program is started (`install`), so every end-to-end check drives the
NEW command line. The OpenSSH spelling itself, which still works for one release,
is exercised by test_cli.sh.

The translation follows the old parsers' own rules: clustered flags, attached
values (`-p22`), `--`, and "everything after the host is the command".
"""
import subprocess


def _take(args, i, letters):
    """The value of a value option at args[i]: attached or the next word."""
    word = args[i]
    if len(word) > 2:
        return word[2:], i + 1
    return args[i + 1], i + 2


def ssh(args):
    scalars, lists = [], {}
    host, command = None, []
    i = 0
    ended = False
    tty = 0
    while i < len(args):
        word = args[i]
        if host is not None:
            command.append(word)
            i += 1
            continue
        if ended or not word.startswith("-") or word == "-":
            host = word
            i += 1
            continue
        if word == "--":
            ended = True
            i += 1
            continue
        if word in ("--help", "--version"):
            return [word]
        if word == "-V":
            return ["--version"]
        if word == "-h":
            return ["--help"]
        letter = word[1]
        if letter in "pliFeJoLR":
            value, i = _take(args, i, "pliFeJoLR")
            if letter == "p":
                scalars += ["--port", value]
            elif letter == "l":
                scalars += ["--user", value]
            elif letter == "F":
                scalars += ["--config", value]
            elif letter == "e":
                scalars += ["--escape", value]
            elif letter == "J":
                scalars += ["--jump", value]
            else:
                name = {"i": "--identity", "o": "--option", "L": "--local-forward", "R": "--remote-forward"}[letter]
                lists.setdefault(name, []).append(value)
            continue
        for flag in word[1:]:
            if flag == "t":
                tty += 1
            elif flag == "T":
                scalars.append("--tty-never")
            elif flag == "N":
                scalars.append("--forward-only")
            elif flag == "n":
                scalars.append("--stdin-null")
            elif flag == "q":
                scalars.append("--quiet")
        i += 1
    out = []
    if host is not None:
        out.append(host)
    if command:
        out.append(" ".join(command))
    out += scalars
    if tty >= 2:
        out.append("--tty-force")
    elif tty == 1:
        out.append("--tty")
    for name, values in lists.items():
        out += [name] + values
    return out


def scp(args):
    scalars, lists, names = [], {}, []
    i = 0
    ended = False
    while i < len(args):
        word = args[i]
        if ended or not word.startswith("-") or word == "-":
            names.append(word)
            i += 1
            continue
        if word == "--":
            ended = True
            i += 1
            continue
        if word in ("--help", "--version"):
            return [word]
        if word == "-V":
            return ["--version"]
        if word == "-h":
            return ["--help"]
        letter = word[1]
        if letter in "PiFoJ":
            value, i = _take(args, i, "PiFoJ")
            if letter == "P":
                scalars += ["--port", value]
            elif letter == "F":
                scalars += ["--config", value]
            elif letter == "J":
                scalars += ["--jump", value]
            else:
                lists.setdefault({"i": "--identity", "o": "--option"}[letter], []).append(value)
            continue
        for flag in word[1:]:
            if flag == "r":
                scalars.append("--recursive")
            elif flag == "p":
                scalars.append("--preserve")
            elif flag == "q":
                scalars.append("--quiet")
        i += 1
    out = names + scalars
    for name, values in lists.items():
        out += [name] + values
    return out


def sftp(args):
    scalars, lists, rest = [], {}, []
    i = 0
    while i < len(args):
        word = args[i]
        if not word.startswith("-") or word == "-":
            rest.append(word)
            i += 1
            continue
        if word in ("--help", "--version"):
            return [word]
        if word == "-V":
            return ["--version"]
        if word == "-h":
            return ["--help"]
        letter = word[1]
        if letter in "PiFoJb":
            value, i = _take(args, i, "PiFoJb")
            if letter == "P":
                scalars += ["--port", value]
            elif letter == "F":
                scalars += ["--config", value]
            elif letter == "J":
                scalars += ["--jump", value]
            elif letter == "b":
                scalars += ["--batch-file", value]
            else:
                lists.setdefault({"i": "--identity", "o": "--option"}[letter], []).append(value)
            continue
        if word == "-q":
            scalars.append("--quiet")
        i += 1
    out = rest + scalars
    for name, values in lists.items():
        out += [name] + values
    return out


def keygen(args):
    mode = "generate"
    options = {}
    host = ""
    i = 0
    while i < len(args):
        word = args[i]
        if word in ("--help", "--version"):
            return [word]
        if word == "-V":
            return ["--version"]
        if word == "-h":
            return ["--help"]
        if word in ("-y", "-l", "-H", "-p"):
            mode = {"-y": "public", "-l": "fingerprint", "-H": "hash", "-p": "passphrase"}[word]
        elif word == "-q":
            options["quiet"] = True
        elif word in ("-t", "-f", "-N", "-P", "-C", "-a", "-R", "-F"):
            value = args[i + 1]
            i += 1
            if word == "-R":
                mode, host = "remove", value
            elif word == "-F":
                mode, host = "find", value
            else:
                options[word] = value
        i += 1
    out = ["-" + mode]
    if mode in ("remove", "find"):
        out.append(host)
    flag_names = {"-t": "--type", "-f": "--file", "-C": "--comment", "-a": "--rounds"}
    for letter, value in options.items():
        if letter == "quiet":
            if mode == "generate":
                out.append("--quiet")
        elif letter == "-N":
            if mode in ("generate", "passphrase"):
                out.append("--new-passphrase=" + value)
        elif letter == "-P":
            out += ["--passphrase" if mode == "public" else "--old-passphrase", value] if value != "" else ["--passphrase=" if mode == "public" else "--old-passphrase="]
        elif letter == "-t" and mode != "generate":
            pass
        elif letter in ("-C", "-a") and mode not in ("generate", "passphrase"):
            pass
        else:
            out += [flag_names[letter], options[letter]]
    return out


def install(binaries):
    """Make subprocess.Popen (and so subprocess.run) translate the argument list of any program
    in `binaries` ({path: translator}) from OpenSSH's spelling to the new one."""
    real_popen = subprocess.Popen

    def translate(cmd):
        if isinstance(cmd, (list, tuple)) and cmd and cmd[0] in binaries:
            return [cmd[0]] + binaries[cmd[0]](list(cmd[1:]))
        return cmd

    class Popen(real_popen):
        def __init__(self, cmd, *a, **k):
            super().__init__(translate(cmd), *a, **k)

    # subprocess.run starts its child through Popen, so patching Popen alone
    # translates every call exactly once.
    subprocess.Popen = Popen
