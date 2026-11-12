unit PrivateMessage;

interface

const
  WM_USER = $0400;

type
  TMessage = record
    Msg: Cardinal;
    WParam: NativeUInt;
    LParam: NativeInt;
    Result: NativeInt;
  end;

  TFoo = class
  private
    procedure WmUser(var Message: TMessage); message WM_USER;
  end;

implementation

procedure TFoo.WmUser(var Message: TMessage);
begin
  Message.Result := 0;
end;

end.
