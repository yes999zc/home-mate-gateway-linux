# G6 serial helper (emOS has no adbd; this is the only device channel).
# Port name can change on USB re-enumeration: list /dev/cu.usbmodem* and
# pick the one that appears when the device boots (baseline the others).
# Usage: serial_cmd.py "<shell command>" [wait_secs] [port]
#!/usr/bin/env python3
"""Send one command to the emOS USB ACM console and capture the reply.

Usage: serial_cmd.py "<command>" [wait_secs=4] [port=/dev/ttyUSB0]

Opens the port raw, drains pending output, sends the command + newline,
then reads until the wait window closes. The console is mksh over ttyGS0;
replies arrive as echoed text. Nothing else uses the port while this runs.
"""
import os
import sys
import termios
import time

cmd = sys.argv[1]
wait = float(sys.argv[2]) if len(sys.argv) > 2 else 4.0
port = sys.argv[3] if len(sys.argv) > 3 else "/dev/ttyUSB0"

fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
a = termios.tcgetattr(fd)
a[3] &= ~(termios.ECHO | termios.ICANON | termios.ISIG)
termios.tcsetattr(fd, termios.TCSANOW, a)

# Drain anything pending.
deadline = time.time() + 0.5
while time.time() < deadline:
    try:
        if not os.read(fd, 4096):
            break
    except BlockingIOError:
        break

os.write(fd, (cmd + "\n").encode())
out = b""
end = time.time() + wait
while time.time() < end:
    try:
        chunk = os.read(fd, 4096)
        if chunk:
            out += chunk
            end = max(end, time.time() + 0.8)  # activity extends the window
    except BlockingIOError:
        time.sleep(0.15)
os.close(fd)
text = out.decode(errors="replace")
# Strip the echoed command line if present.
first_nl = text.find("\n")
if first_nl != -1 and cmd.split()[0] in text[:first_nl + 1]:
    text = text[first_nl + 1:]
print(text.rstrip())
