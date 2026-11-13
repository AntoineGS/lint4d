unit E2003InactiveBranch;
interface
implementation
procedure P;
begin
  {$IFDEF NOT_DEFINED}
  Zz4 := 1;
  {$ENDIF}
end;
end.
