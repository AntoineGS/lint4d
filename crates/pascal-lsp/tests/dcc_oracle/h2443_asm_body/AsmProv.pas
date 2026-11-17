unit AsmProv;
interface
function AsmValue: Integer;
implementation
uses AsmHelper;
function AsmValue: Integer;
asm
  mov eax, 1
end;
end.
