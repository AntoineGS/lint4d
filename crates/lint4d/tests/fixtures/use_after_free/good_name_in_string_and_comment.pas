unit good_name_in_string_and_comment;
interface
implementation
procedure Log(const Msg: string);
begin
end;
procedure Test;
var
  Obj: TObject;
begin
  Obj := TObject.Create;
  Obj.Free;
  Log('Obj was freed');
  Log({ Obj } 'done');
end;
end.
