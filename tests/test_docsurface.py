"""Published-surface docs lock (doctrine 0.1.7 sweep, made permanent).

Every public callable a user can ``help()`` on must carry a non-empty docstring —
``Document.open`` shipped with none for at least one release, invisible in the source
because the defect only exists on the *installed* surface. The scan walks the Python
package's public modules and every public class/function/method **defined there**
(imports are the defining module's responsibility)."""
import inspect

import distillpdf
from distillpdf import cli, document, dpdf, ocr, shell

MODULES = (distillpdf, cli, document, dpdf, ocr, shell)


def _defined_here(obj, mod):
    return getattr(obj, "__module__", None) == mod.__name__


def _empty(obj):
    return not (getattr(obj, "__doc__", None) or "").strip()


def test_every_public_callable_has_a_docstring():
    missing = []
    for mod in MODULES:
        for name in dir(mod):
            if name.startswith("_"):
                continue
            obj = getattr(mod, name)
            if inspect.isclass(obj) and _defined_here(obj, mod):
                if _empty(obj):
                    missing.append(f"{mod.__name__}.{name}")
                for mname, member in vars(obj).items():
                    if mname.startswith("_"):
                        continue
                    fn = getattr(obj, mname)
                    if callable(fn) and _empty(fn):
                        missing.append(f"{mod.__name__}.{name}.{mname}")
            elif inspect.isfunction(obj) and _defined_here(obj, mod) and _empty(obj):
                missing.append(f"{mod.__name__}.{name}")
    assert not missing, f"public items with empty __doc__: {sorted(set(missing))}"
