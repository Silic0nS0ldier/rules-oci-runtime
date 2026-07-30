load("@rules_oci//oci:defs.bzl", "oci_load")
load("@rules_shell//shell:sh_binary.bzl", "sh_binary")

def _label_to_rlocation(label):
    # type: (Label) -> str
    rlocation_parts = [label.repo_name if label.repo_name != "" else "_main"]
    if label.package != "":
        rlocation_parts.append(label.package)
    rlocation_parts.append(label.name)
    return "/".join(rlocation_parts)

def _runc_binary_template_impl(ctx):
    # type: (ctx) -> None
    image_loader_rlocation = _label_to_rlocation(ctx.attr.image_loader.label) + ".sh"
    undocker_rlocation = _label_to_rlocation(ctx.attr._undocker.label) + "_/undocker"
    runc_rlocation = _label_to_rlocation(ctx.attr._runc.label)
    jq_rlocation = _label_to_rlocation(ctx.attr._jq.label)

    runner = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.expand_template(
        template = ctx.file._template,
        output = runner,
        substitutions = {
            "{{image_loader_rlocation}}": image_loader_rlocation,
            "{{undocker_rlocation}}": undocker_rlocation,
            "{{runc_rlocation}}": runc_rlocation,
            "{{jq_rlocation}}": jq_rlocation,
        },
    )

    return [
        DefaultInfo(
            files = depset([runner]),
        )
    ]

_runc_binary_template = rule(
    implementation = _runc_binary_template_impl,
    attrs = {
        # use `oci_load` behind the scenes
        # need a symbolic macro
        "image_loader": attr.label(
            executable = True,
            cfg = config.target(),
            doc = "For label to rlocation conversion",
        ),
        "_template": attr.label(
            default = "//lib:runc_binary.tmpl.sh",
            allow_single_file = True,
            cfg = config.target(),
        ),
        "_undocker": attr.label(
            default = "//third_party/undocker",
            cfg = config.target(),
            doc = "For label to rlocation conversion",
        ),
        "_runc": attr.label(
            default = "@multitool//tools/runc",
            cfg = config.target(),
            doc = "For label to rlocation conversion",
        ),
        "_jq": attr.label(
            default = ":jq",
            cfg = config.target(),
            doc = "For label to rlocation conversion",
        ),
    },
)

def _runc_binary_macro_impl(name, image, visibility):
    oci_load(
        name = name + "_oci_load",
        image = image,
        repo_tags = ["latest"],
        loader = Label("//lib/private/container_import:container_import"),
    )
    _runc_binary_template(
        name = name + "_template",
        image_loader = ":" + name + "_oci_load",
    )
    sh_binary(
        name = name,
        srcs = [name + "_template"],
        data = [
            ":" + name + "_oci_load",
            Label("//third_party/undocker"),
            Label("@multitool//tools/runc"),
            Label(":jq"),
        ],
        use_bash_launcher = True,
        visibility = visibility,
    )

runc_binary = macro(
    implementation = _runc_binary_macro_impl,
    attrs = {
        "image": attr.label(),
    },
)
