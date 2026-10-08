import importlib.machinery
import importlib.metadata
import pathlib
import sys
from types import SimpleNamespace


project_site_packages = sys.argv.pop(1)


def project_distribution(name):
    try:
        return next(
            iter(
                importlib.metadata.distributions(
                    name=name,
                    path=[project_site_packages],
                )
            )
        )
    except StopIteration:
        raise importlib.metadata.PackageNotFoundError(name) from None


def project_distributions():
    return importlib.metadata.distributions(path=[project_site_packages])


def record_top_level(path):
    parts = pathlib.PurePosixPath(path).parts
    if not parts:
        return None
    first = parts[0]
    if "/" in path or first.endswith(".py"):
        return first.removesuffix(".py")
    if first.endswith((".so", ".pyd", ".dll")):
        return first.split(".", 1)[0]
    return None


def build_module_distribution_map():
    result = {}
    for distribution in project_distributions():
        top_level_text = distribution.read_text("top_level.txt")
        if top_level_text is not None:
            module_names = (line.strip() for line in top_level_text.splitlines())
        else:
            record = distribution.read_text("RECORD") or ""
            module_names = (
                record_top_level(line.split(",", 1)[0]) for line in record.splitlines()
            )
        for module_name in module_names:
            if module_name and not module_name.endswith(".dist-info"):
                result.setdefault(module_name, distribution)
    return result


module_distributions = None


def project_module_metadata(name):
    global module_distributions
    try:
        return project_distribution(name).metadata
    except importlib.metadata.PackageNotFoundError:
        if module_distributions is None:
            module_distributions = build_module_distribution_map()
        try:
            return module_distributions[name].metadata
        except KeyError:
            raise importlib.metadata.PackageNotFoundError(name) from None


from deptry.cli import deptry
import deptry.dependency as deptry_dependency
import deptry.module as deptry_module

if not callable(getattr(deptry_dependency.metadata, "distribution", None)):
    raise RuntimeError(
        "the installed deptry is incompatible: dependency metadata lookup is unavailable"
    )
if not callable(getattr(deptry_module, "metadata", None)):
    raise RuntimeError("the installed deptry is incompatible: module metadata lookup is unavailable")
if not callable(getattr(deptry_module, "find_spec", None)):
    raise RuntimeError("the installed deptry is incompatible: module lookup is unavailable")

deptry_dependency.metadata = SimpleNamespace(
    distribution=project_distribution,
    PackageNotFoundError=importlib.metadata.PackageNotFoundError,
)
deptry_module.metadata = project_module_metadata
deptry_module.find_spec = lambda name: importlib.machinery.PathFinder.find_spec(
    name, [project_site_packages]
)

deptry()
