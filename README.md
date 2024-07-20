# NixWindowsVMLauncher
Launcher program to start a windows vm for my nix system.

### How it works:

The program is split into 3 programs

- A root server, which is built to be a systemd service. It handles actually preparing and launching the vm

- A user server, which is built to be a user systemd service. It handles launching vm viewing software, such as looking glass or virt-viewer. It also informs the root server when user session are created.

- A cli program, which is used to tell the root server to start or stop vm's

The root server requires 2 environment variables, WINDOWS_LG_XML and WINDOWS_SPICE_XML, which are paths to xml files containing vm speicification with a looking glass setup and spice setup respectively. These xml files must also contain an evdev mouse device with a file location placeholder: VIRTUAL_MOUSE_EVENT_PATH. The root server automatically relaces this with the correct event path during setup.

The root server also does not start the vm until a user logs in, after the display manager is restarted. This is to prevent the pc from doing costly work when no one is even using the vm.

The program requires TrackpadEvdevConverter to be used as well, and setup as a systemd service. It uses this service to create a virtual mouse for the vm.

The user service should be wanted by graphical-session.target, and is partOf graphical-session.target. This ensures that it is always running with the most up to date value of xauthority.
