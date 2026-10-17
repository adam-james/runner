import contextlib
import io
import json
import traceback

# componentize-py only bundles the standard library modules that are
# imported while it builds the component, so these are here for scripts to
# use, not for this file. Networking and process modules are left out on
# purpose: the sandbox would deny them anyway.
import array, base64, bisect, calendar, cmath, collections, copy, csv  # noqa: E401,F401
import dataclasses, datetime, decimal, difflib, enum, fnmatch, fractions  # noqa: E401,F401
import functools, hashlib, heapq, html, itertools, math, operator, pprint  # noqa: E401,F401
import random, re, shlex, statistics, string, struct, textwrap, typing  # noqa: E401,F401
import unicodedata, urllib.parse, uuid, xml.etree.ElementTree, zlib  # noqa: E401,F401

from componentize_py_types import Err
from wit_world import exports
from wit_world.exports.tool_api import ToolDescription

# Printed output beyond this is cut, so a runaway print loop can't flood the
# model's context.
MAX_OUTPUT_CHARS = 20_000


def truncate(text: str) -> str:
    if len(text) <= MAX_OUTPUT_CHARS:
        return text
    return text[:MAX_OUTPUT_CHARS] + f"\n... [cut: {len(text) - MAX_OUTPUT_CHARS} more chars]"


class ToolApi(exports.ToolApi):
    def describe(self) -> ToolDescription:
        return ToolDescription(
            name="run_python",
            description=(
                "Run a self-contained Python 3 script in a sandbox and get back what it "
                "prints (stdout and stderr). Standard library only, no network access, and "
                "nothing carries over between calls. Use print() to return results."
            ),
            input_schema_json=json.dumps({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Python source code to run as a script",
                    },
                },
                "required": ["code"],
            }),
        )

    def call(self, input_json: str) -> str:
        try:
            code = json.loads(input_json)["code"]
        except (ValueError, KeyError, TypeError):
            raise Err("input must be a JSON object with a string 'code'")
        if not isinstance(code, str):
            raise Err("'code' must be a string")

        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            try:
                exec(compile(code, "<script>", "exec"), {"__name__": "__main__"})
            except SystemExit as e:
                failed = e.code not in (None, 0)
            except BaseException as e:
                failed = True
                # Skip this file's own frame: the model only needs its script's.
                traceback.print_exception(e.with_traceback(e.__traceback__.tb_next))
            else:
                failed = False

        result = truncate(output.getvalue())
        if failed:
            raise Err(result)
        return result or "(no output)"
