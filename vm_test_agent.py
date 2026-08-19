import asyncio
from typing import Optional, Union, TypeVar, Generic
import socket
import typing
import sys
from enum import Enum
import os
import stat
import struct

class Opcode(Enum):
    RUN = 1
    WRITE = 2
    CLOSE = 3

class ResponseCode(Enum):
    SUCCESS = 1
    INVALID_STATE = 2
    NOT_FOUND = 3
    CLOSED = 4
    FAILED = 5
    UNKNOWN_OP = 6
    OUTPUT = 7
    SIGCHILD = 8
    CLOSE = 9

class AttrName(Enum):
    COMMAND = 1
    MESSAGE = 2
    PID = 3
    DATA = 4
    STDERR = 5
    EXIT_CODE = 6

class Attr:
    name: AttrName
    buf: bytes

    def __init__(self, name: AttrName, buf: bytes = b''):
        self.name = name
        self.buf = buf

    def __str__(self):
        return f'<Attr name={self.name} buf={self.buf}>'

    def __repr__(self):
        return str(self)

    @classmethod
    def from_string(klass, name: AttrName, d: Union[str, bytes] = b''):
        if isinstance(d, str):
            d = d.encode('utf-8')

        return klass(name, d)

    def from_int(klass, name: AttrName, d: int, size:int=4):
        return klass(name, d.to_bytes(size, byteorder=sys.byteorder))

    def encode(self) -> bytes:
        return struct.pack('HH', self.name.value, len(self.buf)) + self.buf

IntSize = TypeVar('IntSize')
class IntAttr(Generic[IntSize]):
    name: AttrName
    value: int

    def __init__(self, nm: AttrName, value: int):
        self.name = nm
        self.value = value

    def __str__(self):
        return f'<IntAttr name={self.name} value={self.value}>'

    def __repr__(self):
        return str(self)

    @property
    def pid(self):
        return self.value

    @classmethod
    async def from_stream(klass, name, len, stream):
        try:
            base, = typing.get_args(klass)
        except ValueError:
            base = 32
        byte_count = (base + 7) // 8
        assert byte_count == len, 'Integer attr did not have expected byte count'

        d = await stream.readexactly(len)
        return klass(name, int.from_bytes(d, byteorder=sys.byteorder))

class Message:
    op: Opcode
    attrs: list[Attr]

    def __init__(self, op: Opcode, attrs: Optional[list[Attr]] = None):
        if attrs is None:
            attrs = []
        self.op = op
        self.attrs = attrs

    def add_attr(self, attr: Attr):
        self.attrs.append(attr)

    def encode_message(self) -> bytes:
        header = struct.pack('HBB', self.op.value, len(self.attrs), 0)
        header += b''.join(attr.encode() for attr in self.attrs)
        return header

ATTR_CLASSES = {
    AttrName.PID: IntAttr[32],
    AttrName.EXIT_CODE: IntAttr[32]
}

class Response:
    code: ResponseCode
    attrs: list[Attr]

    def __init__(self, code: ResponseCode, attrs: Optional[list[Attr]] = None):
        self.code = code
        if attrs is None:
            attrs = []
        self.attrs = attrs

    def __repr__(self):
        return str(self)

    def __str__(self):
        attrs = ' '.join(str(a) for a in self.attrs)
        return f'<Response code={self.code} {attrs}>'

    def find_attr(self, attr: AttrName):
        try:
            return next(self.find_attrs(attr))
        except StopIteration:
            return None

    def find_attrs(self, attr: AttrName):
        for a in self.attrs:
            if a.name == attr:
                yield a

    def throw(self):
        if self.code in (ResponseCode.SUCCESS, ResponseCode.OUTPUT):
            return

        elif self.code == ResponseCode.NOT_FOUND:
            raise NotFoundError()
        else:
            raise RuntimeError(f'Bad response {self.code}')

    @classmethod
    async def from_stream(klass, stream, d: Optional[bytes]=None):
        if d is None:
            d = await stream.readexactly(struct.calcsize('HBB'))
        code, num_attrs, pad = struct.unpack('HBB', d)
        assert pad == 0, "Invalid padding"

        code = ResponseCode(code)
        attrs = []
        for _ in range(num_attrs):
            d = await stream.readexactly(struct.calcsize('HH'))
            name, len = struct.unpack('HH', d)
            name = AttrName(name)
            attrkls = ATTR_CLASSES.get(name)
            if attrkls is None:
                attrs.append(Attr(name, await stream.readexactly(len)))
            else:
                attrs.append(await attrkls.from_stream(name, len, stream))

        return klass(code, attrs)

class Command:
    agent: 'VmTestAgent'
    pid: int
    out_queue: asyncio.Queue
    err_queue: asyncio.Queue
    exit_code: asyncio.Future[int]

    def __init__(self, agent: 'VmTestAgent', pid: int):
        self.agent = agent
        self.pid = pid
        self.out_queue = asyncio.Queue()
        self.err_queue = asyncio.Queue()
        self.exit_code = asyncio.get_running_loop().create_future()

    async def write(self, buf: Union[str,bytes]):
        if isinstance(buf, str):
            buf = buf.encode('utf-8')

        assert len(buf) < 16384, f'Buffer of length {len(buf)} cannot be written out in one message'

        msg = Message(Opcode.WRITE, [Attr.from_int(AttrName.PID, self.pid),
                                     Attr.from_string(AttrName.DATA, buf)]).encode_message()

        self.agent.cmdqueue.put((msg, None))

    async def wait(self):
        return await self.exit_code

class VmTestAgent:
    commands: dict[int, Command]
    stdouts: dict[int, Command]
    stderrs: dict[int, Command]
    cmdqueue: asyncio.Queue

    def __init__(self, stream):
        self.stream = stream
        self.commands = {}
        self.stdouts = {}
        self.stderrs = {}
        self.cmdqueue = asyncio.Queue()

    @classmethod
    async def open(klass,
                   filename: Optional[str]=None,
                   fd: Optional[int]=None,
                   vsock: Optional[tuple[int,int]]=None,
                   stream=None):
        if vsock is not None:
            stream = await klass._open_vsock(vsock)
        elif filename is not None:
            stream = await klass._open_file(filename)
        elif fd is not None:
            stream = await klass._open_fd(fd)
        x = klass(stream)
        x.task = asyncio.create_task(x.run())
        return x

    @classmethod
    async def _open_vsock(klass, addr: tuple[int, int]):
        s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
        s.connect(addr)
        return await klass._open_socket(s)

    @classmethod
    async def _open_socket(klass, s: socket.socket):
        s.setblocking(False)
        return await asyncio.open_connection(sock=s)

    @staticmethod
    async def _open_file(filename: str):
        '''Open a socket or char dev'''
        st = os.stat(filename)
        if stat.S_ISSOCK(st.st_mode):
            return await asyncio.open_unix_connection(filename)
        elif stat.S_ISCHR(st.st_mode):
            fd = os.open(filename, os.O_RDWR | os.O_NONBLOCK)
            return await self._open_fd(fd)
        else:
            raise RuntimeError("File must either be a socket or char dev")

    @staticmethod
    async def _open_fd(fd: int):
        loop = asyncio.get_running_loop()
        f = os.fdopen(fd, "r+b", buffering=0)
        reader = asyncio.StreamReader()
        read_protocol = asyncio.StreamReaderProtocol(reader)

        read_transport, _ = await loop.connect_read_pipe(
            lambda: read_protocol,
            f,
        )

        write_transport, write_protocol = await loop.connect_write_pipe(
            asyncio.streams.FlowControlMixin,
            f,
        )

        writer = asyncio.StreamWriter(
            write_transport,
            write_protocol,
            reader,
            loop,
        )

        return reader, writer


    async def run(self):

        def new_read_task():
            return asyncio.create_task(self.read_stream.readexactly(struct.calcsize('HBB')))
        def new_cmd_task():
            return asyncio.create_task(self.cmdqueue.get())
        resp = None
        read_task = new_read_task()
        cmd_task = new_cmd_task()
        while True:
            read_command = []
            if resp is None:
                read_command = [cmd_task]

            nextstep, _ = await asyncio.wait([
                read_task,
                *read_command,
            ], return_when=asyncio.FIRST_COMPLETED)


            if read_task in nextstep:
                try:
                    response = await Response.from_stream(self.read_stream, read_task.result())
                except asyncio.IncompleteReadError:
                    # Shutting down
                    read_task = None
                else:
                    if response.code in (ResponseCode.OUTPUT, ResponseCode.CLOSE):
                        # Look up the PID attr
                        commands = self.stdouts
                        queue_attr = 'out_queue'
                        if response.find_attr(AttrName.STDERR) is None:
                            commands = self.stderrs
                            queue_attr = 'err_queue'

                        if (pid := response.find_attr(AttrName.PID)) is None:
                            print("Received data without pid")
                        elif not isinstance(pid, IntAttr) or pid.pid not in commands:
                            print(f'Received pid {pid.pid}, but no command corresponds')
                        else:
                            cmd = commands[pid.pid]
                            q = getattr(cmd, queue_attr)
                            if response.code == ResponseCode.OUTPUT:
                                d = response.find_attr(AttrName.DATA)
                                if d is None:
                                    print(f'Received output for pid {pid} with no data')
                                else:
                                    await q.put(d.buf)
                            else:
                                await q.put(None)

                    elif response.code == ResponseCode.SIGCHILD:
                        if (pid := response.find_attr(AttrName.PID)) is None:
                            print("Received SIGCHLD without pid")
                        elif not isinstance(pid, IntAttr) or pid.pid not in self.commands:
                            print(f'Received pid {pid.pid}, but no command corresponds')
                        else:
                            code = response.find_attr(AttrName.EXIT_CODE)
                            if not hasattr(code, 'value') or not isinstance(code.value, int):
                                print(f'Exit code for pid {pid.pid} is malformed')
                            else:
                                self.commands[pid.pid].exit_code.set_result(code.value)
                    else:
                        assert resp is not None, f"Response was received but no receiver here... {response}"
                        resp.set_result(response)
                        resp = None
                        cmd_task = new_cmd_task()

                    read_task = new_read_task()
            if cmd_task in nextstep:
                cmd, nextresp = cmd_task.result()
                self.write_stream.write(cmd)
                await self.write_stream.drain()

                if nextresp is not None:
                    assert resp is None, 'We are reading the command queue despite having an outstanding command'
                    resp = nextresp
                cmd_task = None

    async def run_command(self, args: Union[str, list[str]],
                           env: Optional[dict[str, str]] = None):
        msg = Message(Opcode.RUN,
                      [Attr.from_string(AttrName.COMMAND, arg) for arg in args]) \
                .encode_message()
        loop = asyncio.get_running_loop()
        fut = loop.create_future()
        await self.cmdqueue.put((msg, fut))
        resp = await fut
        resp.throw()

        pid = resp.find_attr(AttrName.PID)
        assert pid is not None, f'Run response contained no PID'

        cmd = self.commands[pid.pid] = self.stderrs[pid.pid] = self.stdouts[pid.pid] = Command(self, pid.pid)
        return cmd

    @property
    def write_stream(self):
        return self.stream[1]

    @property
    def read_stream(self):
        return self.stream[0]

    async def close(self):
        await self.read_stream.close()
        await self.write_stream.close()

async def dump_out(queue, file):
    while True:
        buf = await queue.get()
        if buf is None:
            return
        file.write(buf)

async def go(opts):
    if opts.vsock:
        open_args = dict(vsock=(int(opts.file), opts.port))
    else:
        open_args = dict(filename=opts.file)
    agent = await VmTestAgent.open(**open_args)
    cmd = await agent.run_command(opts.cmd)

    asyncio.create_task(dump_out(cmd.out_queue, sys.stdout.buffer))
    asyncio.create_task(dump_out(cmd.err_queue, sys.stderr.buffer))
    code = await cmd.wait()
    cmd = await agent.run_command(opts.cmd)

    asyncio.create_task(dump_out(cmd.out_queue, sys.stdout.buffer))
    asyncio.create_task(dump_out(cmd.err_queue, sys.stderr.buffer))
    code = await cmd.wait()

    return code

if __name__ == "__main__":
    from argparse import ArgumentParser
    args = ArgumentParser(prog="vm-test-agent",
                          description="Run commands in a vm-test-agent")
    args.add_argument("file", help="Socket or Chardev to connect to", metavar="SOCK")
    args.add_argument("cmd", help="Command to run", metavar="COMMAND", nargs='+')
    args.add_argument("--vsock", help="Interpret SOCK as a vsock number", action='store_true', default=False)
    args.add_argument("--port", help="Port to connect on", type=int, default=5757)
    opts = args.parse_args()

    sys.exit(asyncio.run(go(opts)))

