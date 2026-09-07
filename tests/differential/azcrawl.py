#!/usr/bin/env python3
"""Write the `crawl_metadata` fixtures both sides of `azure.py crawl` read.

A fixture stands in for the machine: which provisioning media exist and what
they hold, whether DHCP came up, what IMDS answered, and what wireserver said.
Neither implementation touches a host, so the comparison is of the sequencing
and of the four documents the crawl returns.

Usage: azcrawl.py <output-dir>
"""
import json
import os
import sys

UUID = "8c9e6a3a-1b2c-4d5e-8f90-0a1b2c3d4e5f"
DDIR = "/var/lib/waagent"
SEED = "/var/lib/cloud/seed/azure"
ISO = "/dev/sr0"


def compute(**fields):
    return {"compute": fields}


def cases(ovf):
    """The fixtures, by name. `ovf` is the document read from disk."""
    proxy_ovf = ovf.replace(
        "<PreprovisionedVm>false</PreprovisionedVm>",
        "<PreprovisionedVm>false</PreprovisionedVm>\n"
        "   <ProvisionGuestProxyAgent>true</ProvisionGuestProxyAgent>",
    )
    imds_host = compute(osProfile={"computerName": "imds-host"})

    def pps(kind, **extra):
        body = dict(
            system_uuid=UUID,
            networking_up=True,
            imds={
                "compute": {"osProfile": {"computerName": "vm"}},
                "extended": {"compute": {"ppsType": kind}},
            },
        )
        body.update(extra)
        return body

    return {
        # Neither provisioning media nor IMDS: the one fatal combination.
        "nothing": dict(system_uuid=UUID, networking_up=True),
        # No lease at all, so IMDS is never asked and nothing answers.
        "no-lease": dict(system_uuid=UUID, networking_up=False),
        # The VM cannot be identified, which stops the crawl before anything
        # else is read.
        "no-uuid": dict(networking_up=True, imds=imds_host),
        # IMDS alone: the user, the hostname and the OVF are all synthesised
        # from it.
        "imds-only": dict(
            system_uuid=UUID,
            networking_up=True,
            imds=compute(
                osProfile={
                    "adminUsername": "azureuser",
                    "computerName": "vm-1",
                    "disablePasswordAuthentication": "true",
                }
            ),
            report_ready=["ssh-rsa AAAAKEY"],
        ),
        # The same, with password authentication left enabled.
        "imds-pwauth": dict(
            system_uuid=UUID,
            networking_up=True,
            imds=compute(
                osProfile={
                    "adminUsername": "user2",
                    "computerName": "vm-2",
                    "disablePasswordAuthentication": "false",
                }
            ),
        ),
        # User data from IMDS, taken because no OVF supplied any.
        "imds-userdata": dict(
            system_uuid=UUID,
            networking_up=True,
            imds=compute(
                osProfile={"computerName": "vm"},
                userData="ZWNobyBmcm9tLWltZHMK",
            ),
        ),
        # Malformed IMDS user data is dropped rather than fatal.
        "bad-userdata": dict(
            system_uuid=UUID,
            networking_up=True,
            imds=compute(
                osProfile={"computerName": "vm"}, userData="!!!not base64"
            ),
        ),
        # Media on the ISO answers first, so the data dir is never read -- but
        # IMDS still overrides the hostname, and does not override user data.
        "iso-wins": dict(
            system_uuid=UUID,
            networking_up=True,
            candidates=[
                {"kind": "dir", "path": SEED},
                {"kind": "device", "path": ISO},
                {"kind": "dir", "path": DDIR},
            ],
            sources={ISO: ovf},
            imds=compute(
                osProfile={"computerName": "imds-host"},
                userData="aWdub3JlZA==",
            ),
            report_ready=["ssh-rsa FROMWIRE"],
        ),
        # A cached data dir needs no mount, so only five minutes are spent
        # waiting for a lease rather than twenty.
        "cached-ddir": dict(
            system_uuid=UUID,
            networking_up=True,
            candidates=[
                {"kind": "dir", "path": SEED},
                {"kind": "dir", "path": DDIR},
            ],
            sources={DDIR: ovf},
            imds=imds_host,
        ),
        # A device that will not mount is stepped over, not fatal.
        "unmountable": dict(
            system_uuid=UUID,
            networking_up=True,
            candidates=[
                {"kind": "device", "path": ISO},
                {"kind": "dir", "path": DDIR},
            ],
            unmountable=[ISO],
            sources={DDIR: ovf},
            imds=imds_host,
        ),
        # The seed directory answers, which is neither a mount nor the cache.
        "seed-dir": dict(
            system_uuid=UUID,
            networking_up=True,
            candidates=[
                {"kind": "dir", "path": SEED},
                {"kind": "dir", "path": DDIR},
            ],
            sources={SEED: ovf},
            imds=imds_host,
        ),
        # Wireserver refuses; the crawl carries on without its keys and leaves
        # the markers in place.
        "report-fails": dict(
            system_uuid=UUID,
            networking_up=True,
            imds=compute(osProfile={"computerName": "vm"}),
            report_ready_error="wireserver unreachable",
        ),
        # A boot that already reported ready does not report again.
        "negotiated": dict(
            system_uuid=UUID,
            networking_up=True,
            negotiated=True,
            imds=compute(osProfile={"computerName": "vm"}),
            report_ready=["ssh-rsa NEVER"],
        ),
        # A previous boot recorded an instance id, which is kept.
        "previous-iid": dict(
            system_uuid=UUID,
            networking_up=True,
            previous_instance_id=UUID.upper(),
            imds=compute(osProfile={"computerName": "vm"}),
        ),
        "random-seed": dict(
            system_uuid=UUID,
            networking_up=True,
            random_seed="c2VlZA==",
            imds=compute(osProfile={"computerName": "vm"}),
        ),
        # A gen1 guest, whose VM id is the byte-swapped system uuid.
        "gen1": dict(
            system_uuid=UUID,
            gen1=True,
            networking_up=True,
            imds=compute(osProfile={"computerName": "vm"}),
        ),
        # The OVF opts into the proxy agent, so its status is checked before
        # IMDS is read.
        "proxy-agent": dict(
            system_uuid=UUID,
            networking_up=True,
            candidates=[{"kind": "dir", "path": DDIR}],
            sources={DDIR: proxy_ovf},
            imds=imds_host,
        ),
        # Every pre-provisioning shape, which the port refuses.
        "pps-savable": pps("Savable"),
        "pps-running": pps("Running"),
        "pps-osdisk": pps("PreprovisionedOSDisk"),
        "pps-unknown": pps("Whatever"),
        # Source PPS is a hard failure without networking, and that check
        # comes before the reprovisioning wait.
        "pps-no-lease": dict(
            system_uuid=UUID,
            networking_up=False,
            candidates=[{"kind": "dir", "path": DDIR}],
            sources={
                DDIR: ovf.replace(
                    "<PreprovisionedVm>false</PreprovisionedVm>",
                    "<PreprovisionedVm>true</PreprovisionedVm>",
                )
            },
        ),
    }


def main(argv):
    if not argv:
        print("usage: azcrawl.py <output-dir>", file=sys.stderr)
        return 2
    out = argv[0]
    with open(os.path.join(out, "ovf.xml")) as handle:
        ovf = handle.read()

    for name, body in cases(ovf).items():
        body.setdefault("data_dir", DDIR)
        with open(os.path.join(out, "%s.json" % name), "w") as handle:
            json.dump(body, handle, indent=2, sort_keys=True)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
