unit bad_assignment_reads_freed;
interface
implementation
procedure Test;
var
  Obj: TObject;
  Name: string;
begin
  Obj := TObject.Create;
  Obj.Free;
  Name := Obj.ClassName;
end;
end.
