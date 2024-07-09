# NixWindowsVMLauncher
Launcher program to start a windows vm for my nix system.



Todo:

Detect User logins without the use of a servant

Stop app if vm is closed, or if all users close the viewing client, shutdown vm and close

get rid of servant app

Run user apps from root process

Program gets launched
If not root, return err

Split based on LG or Spice

LG:

Shutdown Display Manager: uses dbus, system connection
Find all Active Users: use login1
stop pipewire.socket and pipewire-pulse.socket for all users: use set effective id for open and register, can switch back after
Wait for pipewire, pulse, and display-manager to be fully unloaded: use dbus, job query
Unload modules, disconnect, load vfio-> requires modprobe and virsh
Restart pipewire, use previous connections
Restart display manager, system bus
Wait for at least 1 user to log in: use dbus connection to login1, wait for signal of new session

Finish Setup, Spice jumps to here

create mouse
easy performance
finish xml
launch vm
launvh vm viewer
wait for all vm viewers to shutdown, then shutdown vm, or wait for vm to shutdown, and close all viewers
disable mouse
undo performance

LG:

Undoes setup