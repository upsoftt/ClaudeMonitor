Dim fso, dir, py, script
Set fso = CreateObject("Scripting.FileSystemObject")
dir    = fso.GetParentFolderName(WScript.ScriptFullName)
py     = dir & "\.venv\Scripts\pythonw.exe"
script = dir & "\usage_monitor.py"
CreateObject("WScript.Shell").Run Chr(34) & py & Chr(34) & " " & Chr(34) & script & Chr(34), 0, False
